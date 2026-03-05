/// HIR-to-JS codegen.
///
/// Produces memoized JS output from a HIRFunction + Environment.
///
/// Core strategy:
///   1. Walk blocks in order (entry → exit via fallthrough chain).
///   2. Sort `env.scopes` by range.start to assign sequential cache slots.
///   3. Build an inlining map: for each instruction whose lvalue is a temp that
///      has no name, compute a fully-inlined JS expression string. This allows
///      `t0 = someObj()` rather than `const $t16 = someObj(); t0 = $t16;`.
///   4. For each instruction:
///      - If it's transparent (load/inline), skip statement emission.
///      - If it's a scope output, handle via `tN = <expr>` pattern.
///      - Otherwise emit as a local binding.
///   5. Emit `const $ = _c(N)` + memoization if/else blocks.

use std::collections::HashMap;
use std::fmt::Write;

use crate::hir::hir::{
    ArrayElement, BlockId, BinaryOperator, CallArg, DeclarationId, FunctionExpressionType,
    GotoVariant, HIRFunction, Instruction, InstructionId, InstructionKind, InstructionValue,
    JsxAttribute, JsxTag, NonLocalBinding, ObjectExpressionProperty, ObjectProperty,
    ObjectPropertyKey, ObjectPatternProperty, Param, Pattern, Place, PrimitiveValue,
    ReactiveScope, ReactiveScopeDependency, ScopeId, Terminal, UnaryOperator, UpdateOperator,
};
use crate::hir::environment::Environment;

// ---------------------------------------------------------------------------
// HTML entity decoding for JSX text
// ---------------------------------------------------------------------------

/// Decode HTML entities commonly found in JSX text nodes.
/// Handles named entities: &amp; &lt; &gt; &quot; &apos; and numeric forms &#N; &#xN;
fn decode_jsx_html_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '&' {
            // Find the semicolon end within reasonable range.
            let rest: String = chars[i..].iter().collect();
            if let Some(end_rel) = rest.find(';') {
                let entity = &rest[..end_rel + 1];
                let decoded = match entity {
                    "&amp;" => Some("&"),
                    "&lt;" => Some("<"),
                    "&gt;" => Some(">"),
                    "&quot;" => Some("\""),
                    "&apos;" => Some("'"),
                    _ => None,
                };
                if let Some(ch) = decoded {
                    out.push_str(ch);
                    i += entity.chars().count();
                    continue;
                }
                // Numeric entity &#N; or &#xN;
                if entity.starts_with("&#") {
                    let inner = &entity[2..entity.len()-1];
                    let code_point = if inner.starts_with('x') || inner.starts_with('X') {
                        u32::from_str_radix(&inner[1..], 16).ok()
                    } else {
                        inner.parse::<u32>().ok()
                    };
                    if let Some(cp) = code_point.and_then(char::from_u32) {
                        out.push(cp);
                        i += entity.chars().count();
                        continue;
                    }
                }
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

pub fn codegen_hir_function(hir: &HIRFunction, env: &Environment) -> String {
    let mut gen = Codegen::new(hir, env);
    let mut out = gen.emit();
    // Emit any functions that were outlined by the outline_functions pass.
    // Apply body text normalization and re-indent to top-level (body_pad = "").
    for (_, decl) in &env.outlined_functions {
        out.push('\n');
        let normalized = normalize_fn_body_text(decl);
        let reindented = reindent_multiline(&normalized, "");
        out.push_str(&reindented);
        out.push('\n');
    }
    out
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Replace whole-word occurrences of `old` with `new` in `text`.
/// A word boundary is any byte that is not alphanumeric, `_`, or `$`.
fn rename_word_in_src(text: &str, old: &str, new: &str) -> String {
    if old.is_empty() || old == new { return text.to_string(); }
    let mut result = String::new();
    let bytes = text.as_bytes();
    let old_bytes = old.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i..].starts_with(old_bytes) {
            let before_ok = i == 0 || {
                let b = bytes[i - 1];
                !(b.is_ascii_alphanumeric() || b == b'_' || b == b'$')
            };
            let after_pos = i + old.len();
            let after_ok = after_pos >= bytes.len() || {
                let b = bytes[after_pos];
                !(b.is_ascii_alphanumeric() || b == b'_' || b == b'$')
            };
            if before_ok && after_ok {
                result.push_str(new);
                i = after_pos;
                continue;
            }
        }
        result.push(bytes[i] as char);
        i += 1;
    }
    result
}

/// Returns true if the given `name` is declared locally within `body` via
/// `let`, `const`, `var`, or a function parameter.
/// This is a heuristic to detect shadowing local declarations so we don't
/// incorrectly rename captured variable references.
fn has_local_decl_in_body(body: &str, name: &str) -> bool {
    // Check for `let name`, `const name`, `var name`, `function name(` in body.
    // Also check for destructuring and `catch (name)`.
    let patterns = [
        format!("let {}", name),
        format!("const {}", name),
        format!("var {}", name),
        format!("catch ({})", name),
        format!("catch({})", name),
    ];
    for pat in &patterns {
        if contains_as_word_codegen(body, pat) {
            return true;
        }
    }
    false
}

/// Apply `name_overrides` to captured variable references in `src`.
///
/// When a variable captured from the outer scope has been renamed (e.g., inner
/// `x` → `x_0` because of shadowing), we must rename all references to that
/// variable in the function body text. The captured variables are enumerated
/// in `context` (each `Place.identifier`).
///
/// We skip renaming if the closure declares the same name locally (which means
/// the closure is NOT actually capturing the outer variable — it has its own).
fn apply_capture_renames(
    src: &str,
    context: &[crate::hir::hir::Place],
    env: &Environment,
    name_overrides: &HashMap<u32, String>,
) -> String {
    let mut result = src.to_string();
    for place in context {
        let id = place.identifier;
        if let Some(new_name) = name_overrides.get(&id.0) {
            // Get original source name.
            if let Some(orig_name) = env.get_identifier(id).and_then(|i| i.name.as_ref()).map(|n| n.value().to_string()) {
                if &orig_name == new_name {
                    continue;
                }
                // If the function body locally declares this name (shadowing the capture),
                // do NOT rename — the body's references are to the local variable.
                if has_local_decl_in_body(&result, &orig_name) {
                    continue;
                }
                result = rename_word_in_src(&result, &orig_name, new_name);
            }
        }
    }
    result
}

/// Word-boundary-safe substring search.
/// Returns true if `pattern` appears in `s` not preceded or followed by an
/// identifier character (alphanumeric, `_`, or `$`).
fn contains_as_word_codegen(s: &str, pattern: &str) -> bool {
    if pattern.is_empty() { return false; }
    let mut start = 0;
    while let Some(rel_pos) = s[start..].find(pattern) {
        let pos = start + rel_pos;
        let before_ok = pos == 0 || {
            let c = s[..pos].chars().next_back().unwrap_or('\0');
            !(c.is_alphanumeric() || c == '_' || c == '$')
        };
        let after_ok = pos + pattern.len() >= s.len() || {
            let c = s[pos + pattern.len()..].chars().next().unwrap_or('\0');
            !(c.is_alphanumeric() || c == '_' || c == '$')
        };
        if before_ok && after_ok {
            return true;
        }
        start = pos + 1;
    }
    false
}

// ---------------------------------------------------------------------------
// Internal state
// ---------------------------------------------------------------------------

/// One output produced by a memoized scope.
///
/// Named-var approach (is_named_var=true): the variable itself is the cache var.
///   `let x; if (...) { x = ...; $[slot] = x; } else { x = $[slot]; }`
///
/// Temp approach (is_named_var=false): a fresh tN temp holds the cached value.
///   `let tN; if (...) { tN = cache_expr; $[slot] = tN; } else { tN = $[slot]; } const x = tN;`
struct ScopeOutputItem {
    /// If Some(i), skip instruction at index i in the scope's instruction slice during body emission.
    skip_idx: Option<usize>,
    /// Expression assigned to the temp (temp approach) or stored directly (named-var approach).
    cache_expr: String,
    /// Named variable to bind after the scope (post-if `const x = tN`) or use as cache_var.
    out_name: Option<String>,
    /// Keyword for the post-if binding (`const` or `let`). Only used by temp approach.
    out_kw: &'static str,
    /// True if the variable is the cache var (named-var approach); false for temp approach.
    is_named_var: bool,
}

/// Complete analysis of a scope's outputs, intra-scope variables, and whether
/// the scope captures the function's return terminal value.
struct ScopeOutput {
    /// All outputs produced by this scope, in instruction order.
    outputs: Vec<ScopeOutputItem>,
    /// Indices of StoreLocal instructions that are "intra-scope" — their bound variable
    /// is only used within the scope. These are emitted inline as `const var = val`
    /// inside the if-block, NOT skipped and NOT hoisted outside.
    intra_scope_stores: Vec<usize>,
    /// If this scope captures the function's return terminal value, the identifier id
    /// of that terminal place. Used to redirect `return expr` → `return tN`.
    terminal_place_id: Option<u32>,
    /// If the terminal feed instruction is a TypeCastExpression, this holds the
    /// annotation string (e.g. "const") so that `return tN as const` is emitted
    /// instead of putting `as const` inside the scope body.
    terminal_type_cast_annotation: Option<String>,
}

/// Identifies which loop type is being wrapped in a scope block.
enum LoopType {
    DoWhile,
    While { test_bid: BlockId },
    ForOf {
        test_bid: BlockId,
        iterable_expr: String,
        loop_var_name: String,
        binding_id: u32,
    },
    ForIn {
        iterable_expr: String,
        loop_var_name: String,
        binding_id: u32,
    },
}

struct Codegen<'a> {
    hir: &'a HIRFunction,
    env: &'a Environment,
    /// inlined_exprs: identifier id → fully-inlined JS expression.
    /// Populated for transparent instructions (loads, property loads, and
    /// single-use temps whose value can be fully inlined).
    inlined_exprs: HashMap<u32, String>,
    /// dep_slots[scope_id] = list of cache-slot indices for each dependency.
    dep_slots: HashMap<ScopeId, Vec<usize>>,
    /// output_slots[scope_id] = list of cache-slot indices for each output (1 or more).
    output_slots: HashMap<ScopeId, Vec<usize>>,
    /// Total number of cache slots (sum of K+N_outputs per scope).
    num_scopes: usize,
    /// Number of promoted param names (t0, t1, ...). Scope result temps
    /// start from this offset so they don't collide with param names.
    param_name_offset: usize,
    /// Instruction map: instr lvalue identifier id → Instruction
    instr_map: HashMap<u32, Instruction>,
    /// Use count: how many times each identifier is used as an operand.
    use_count: HashMap<u32, u32>,
    /// Maps an inlined instruction's identifier id → temp name (tN) that should
    /// replace it in the terminal emitter. Set when a scope captures a return value.
    terminal_replacement: HashMap<u32, String>,
    /// Reverse map: SSA temp identifier id → name of the named variable it flows into.
    /// e.g. if `const a = $t31` then ssa_value_to_name[31] = "a".
    /// Used by dep_expr to emit "a" instead of "$t31" for scope dependencies.
    ssa_value_to_name: HashMap<u32, String>,
    /// Maps instruction id → block id. Used to identify which instructions
    /// are pre-loop vs in-loop-body when emitting loop-wrapped scope blocks.
    instr_to_block: HashMap<InstructionId, BlockId>,
    /// Scopes whose "new branch" is currently being emitted via loop-wrapping.
    /// Instructions belonging to these scopes are emitted as plain statements
    /// (rather than triggering another scope block emission).
    within_loop_scopes: std::collections::HashSet<ScopeId>,
    /// Set of identifier IDs for named variables whose names appear in any InlineJs
    /// source string. InlineJs instructions reference variables by name rather than
    /// via HIR operands, so we precompute this to treat them as "used outside scope".
    inline_js_referenced_ids: std::collections::HashSet<crate::hir::hir::IdentifierId>,
    /// Name overrides for variables that would shadow an already-declared name.
    /// Maps IdentifierId.0 → renamed string (e.g., "x" → "x_0").
    /// Built during Codegen::new() by detecting naming conflicts in program order.
    name_overrides: HashMap<u32, String>,
    /// Maps switch terminal InstructionId → sequential label number (bb0, bb1, ...).
    /// Only populated for switches that need explicit labels (where at least one
    /// case has an explicit Break goto to the switch fallthrough).
    switch_labels: HashMap<InstructionId, u32>,
    /// Maps switch fallthrough BlockId → label string (e.g., "bb0").
    /// Used during emit to produce `break <label>;` inside case bodies.
    switch_fallthrough_labels: HashMap<BlockId, String>,
    /// Maps identifier id → scope output temp name (e.g., "t0", "t1").
    /// Populated during scope emission so that references to scope outputs
    /// outside the scope resolve to the correct temp name instead of "$tN".
    scope_output_names: HashMap<u32, String>,
    /// Set of DeclarationIds that are targets of reassignment or update expressions.
    /// Used to emit `let` instead of `const` for destructuring patterns whose
    /// bound variables are later mutated (e.g., `let { c } = t0` when `c++` follows).
    reassigned_decl_ids: std::collections::HashSet<DeclarationId>,
    /// Set of fallthrough BlockIds that belong to Label terminals (as opposed to switches).
    /// Used to distinguish natural exits (no break needed) from switch exits (break needed).
    label_fallthrough_blocks: std::collections::HashSet<BlockId>,
}

/// Traverse blocks reachable from `start` (not crossing `fall_bid`) and check
/// if any has a Goto(fall_bid, Break) terminal — indicating an explicit `break;`.
fn case_subgraph_has_explicit_break(
    blocks: &indexmap::IndexMap<BlockId, crate::hir::hir::BasicBlock>,
    start: BlockId,
    fall_bid: BlockId,
) -> bool {
    use std::collections::HashSet;
    let mut visited: HashSet<BlockId> = HashSet::new();
    let mut stack = vec![start];
    while let Some(bid) = stack.pop() {
        if bid == fall_bid || !visited.insert(bid) { continue; }
        let Some(block) = blocks.get(&bid) else { continue; };
        if let Terminal::Goto { block: dest, variant, .. } = &block.terminal {
            if *dest == fall_bid && *variant == GotoVariant::Break {
                return true;
            }
        }
        for succ in block.terminal.successors() {
            stack.push(succ);
        }
    }
    false
}

/// Helper: determine if an instruction is assigned to scope `sid`.
/// Mirrors the 3-step logic of `assign_instructions_to_scopes`.
fn instr_in_scope(instr: &Instruction, sid: ScopeId, scope: &ReactiveScope, env: &Environment) -> bool {
    // Step 1: lvalue ident has scope == sid.
    if env.get_identifier(instr.lvalue.identifier).and_then(|i| i.scope).map(|s| s == sid).unwrap_or(false) {
        return true;
    }
    // Step 2: StoreLocal value or target has scope == sid.
    match &instr.value {
        InstructionValue::StoreLocal { lvalue, value, .. } => {
            if env.get_identifier(value.identifier).and_then(|i| i.scope).map(|s| s == sid).unwrap_or(false) {
                return true;
            }
            if env.get_identifier(lvalue.place.identifier).and_then(|i| i.scope).map(|s| s == sid).unwrap_or(false) {
                return true;
            }
        }
        _ => {}
    }
    // Step 3: instruction id within scope range.
    let range_nonempty = scope.range.end > scope.range.start;
    range_nonempty && instr.id >= scope.range.start && instr.id < scope.range.end
}

/// Pre-compute the number of output slots needed per scope.
///
/// An escaping StoreLocal is one whose named variable is loaded OUTSIDE
/// the scope (any LoadLocal instruction not in the scope's instruction set).
/// If no StoreLocals escape, count non-transparent SSA temps consumed outside.
fn count_scope_outputs(hir: &HIRFunction, env: &Environment) -> HashMap<ScopeId, usize> {
    use crate::hir::visitors::{each_instruction_value_operand, each_terminal_operand};
    let mut result: HashMap<ScopeId, usize> = HashMap::new();
    for (&sid, scope) in &env.scopes {
        // Collect instruction ids and lvalue ids assigned to this scope.
        let mut scope_instr_ids: std::collections::HashSet<u32> = std::collections::HashSet::new();
        let mut scope_lvalue_ids: std::collections::HashSet<u32> = std::collections::HashSet::new();
        let mut store_var_ids: Vec<crate::hir::hir::IdentifierId> = Vec::new();
        for (_, block) in &hir.body.blocks {
            for instr in &block.instructions {
                if instr_in_scope(instr, sid, scope, env) {
                    scope_instr_ids.insert(instr.id.0);
                    scope_lvalue_ids.insert(instr.lvalue.identifier.0);
                    if let InstructionValue::StoreLocal { lvalue, value, .. } = &instr.value {
                        // Count StoreLocals where the bound variable OR its stored value
                        // belongs to this scope. The value case handles `const x = push_result`
                        // where push_result has scope==sid but x is not in scope.declarations.
                        let var_id = lvalue.place.identifier;
                        let is_scope_decl = scope.declarations.contains_key(&var_id);
                        let is_scope_ident = env.get_identifier(var_id)
                            .and_then(|i| i.scope)
                            == Some(sid);
                        let val_is_scope_owned = scope.declarations.contains_key(&value.identifier)
                            || env.get_identifier(value.identifier).and_then(|i| i.scope) == Some(sid);
                        if is_scope_decl || is_scope_ident || val_is_scope_owned {
                            store_var_ids.push(var_id);
                        }
                    }
                }
            }
        }
        // Count StoreLocal var_ids whose var is loaded by an instruction NOT in scope.
        // Deduplicate: multiple StoreLocals to the same named variable (e.g. from
        // inlined IIFE bodies or re-assignments) should count as one scope output.
        store_var_ids.sort_unstable_by_key(|id| id.0);
        store_var_ids.dedup();
        let mut n_escaping = 0usize;
        for var_id in &store_var_ids {
            let used_outside = hir.body.blocks.iter().any(|(_, block)| {
                block.instructions.iter().any(|i| {
                    if scope_instr_ids.contains(&i.id.0) { return false; }
                    if let InstructionValue::LoadLocal { place, .. } = &i.value {
                        return place.identifier == *var_id;
                    }
                    false
                })
            });
            if used_outside { n_escaping += 1; }
        }
        if n_escaping > 0 {
            result.insert(sid, n_escaping);
            continue;
        }
        // No escaping StoreLocals. Count non-transparent SSA temps consumed outside scope.
        //
        // Build scope membership using steps 1 and 2 of instr_in_scope, but NOT step 3.
        // Step 3 (range-based) would include hook calls that fall within the scope range
        // but are excluded from the scope block by assign_instructions_to_scopes.
        // By excluding step 3, hook calls at the end of the range are treated as "outside",
        // so their consumed operands (the scope outputs) correctly appear in outside_consumed.
        let mut scope_instr_lvalue_ids: std::collections::HashSet<u32> = std::collections::HashSet::new();
        // Step 1: ident.scope == sid.
        for (&id, ident) in &env.identifiers {
            if ident.scope == Some(sid) {
                scope_instr_lvalue_ids.insert(id.0);
            }
        }
        // Step 2: StoreLocal instructions where value's scope or target's scope == sid.
        for (_, block) in &hir.body.blocks {
            for instr in &block.instructions {
                if let InstructionValue::StoreLocal { lvalue, value, .. } = &instr.value {
                    let v_in = env.get_identifier(value.identifier).and_then(|i| i.scope) == Some(sid);
                    let t_in = env.get_identifier(lvalue.place.identifier).and_then(|i| i.scope) == Some(sid);
                    if v_in || t_in {
                        scope_instr_lvalue_ids.insert(instr.lvalue.identifier.0);
                    }
                }
            }
        }
        // Step 2.5: Propagate scope membership through:
        //   (a) LoadLocal/LoadContext of scope-owned sources — e.g., loading a scope-owned
        //       named variable to pass as the object of PropertyStore. Without this, the
        //       LoadLocal temp is not recognized as scope-owned, so PropertyStore.object
        //       isn't scope-owned, so PropertyStore doesn't get added, and the value being
        //       stored appears to escape outside the scope.
        //   (b) Mutation instructions (PropertyStore, ComputedStore, etc.) where the object
        //       is scope-owned. These are intra-scope mutations, not scope outputs.
        // Apply iteratively until fixed point to handle chained accesses.
        loop {
            let before = scope_instr_lvalue_ids.len();
            for (_, block) in &hir.body.blocks {
                for instr in &block.instructions {
                    // (a) LoadLocal/LoadContext of a scope-owned source.
                    let load_src = match &instr.value {
                        InstructionValue::LoadLocal { place, .. }
                        | InstructionValue::LoadContext { place, .. } => Some(place.identifier.0),
                        _ => None,
                    };
                    if let Some(src) = load_src {
                        if scope_instr_lvalue_ids.contains(&src) {
                            scope_instr_lvalue_ids.insert(instr.lvalue.identifier.0);
                        }
                    }
                    // (b) Mutation of a scope-owned object.
                    let obj_id = match &instr.value {
                        InstructionValue::PropertyStore { object, .. }
                        | InstructionValue::PropertyDelete { object, .. }
                        | InstructionValue::ComputedStore { object, .. }
                        | InstructionValue::ComputedDelete { object, .. } => Some(object.identifier.0),
                        _ => None,
                    };
                    if let Some(oid) = obj_id {
                        if scope_instr_lvalue_ids.contains(&oid) {
                            scope_instr_lvalue_ids.insert(instr.lvalue.identifier.0);
                        }
                    }
                }
            }
            if scope_instr_lvalue_ids.len() == before { break; }
        }
        // Collect identifiers consumed by instructions OUTSIDE this set (and by terminals).
        let mut outside_consumed: std::collections::HashSet<u32> = std::collections::HashSet::new();
        for (_, block) in &hir.body.blocks {
            for instr in &block.instructions {
                if scope_instr_lvalue_ids.contains(&instr.lvalue.identifier.0) { continue; }
                for op in each_instruction_value_operand(&instr.value) {
                    if std::env::var("RC_DEBUG2").is_ok() && scope_instr_lvalue_ids.contains(&op.identifier.0) {
                        eprintln!("[outside] scope {:?} lv={} consumed by instr lv={} ({:?})",
                            sid.0, op.identifier.0, instr.lvalue.identifier.0,
                            std::mem::discriminant(&instr.value));
                    }
                    outside_consumed.insert(op.identifier.0);
                }
            }
            for op in each_terminal_operand(&block.terminal) {
                if std::env::var("RC_DEBUG2").is_ok() && scope_instr_lvalue_ids.contains(&op.identifier.0) {
                    eprintln!("[outside] scope {:?} lv={} consumed by terminal", sid.0, op.identifier.0);
                }
                outside_consumed.insert(op.identifier.0);
            }
        }
        // Count non-transparent scope-owned identifiers consumed outside.
        let mut n_ssa_outputs = 0usize;
        for &lv_id in &scope_instr_lvalue_ids {
            if !outside_consumed.contains(&lv_id) { continue; }
            let instr_opt = hir.body.blocks.values()
                .flat_map(|b| &b.instructions)
                .find(|i| i.lvalue.identifier.0 == lv_id);
            let is_transparent = instr_opt
                .map(|i| matches!(&i.value,
                    InstructionValue::LoadLocal { .. }
                    | InstructionValue::LoadGlobal { .. }
                    | InstructionValue::LoadContext { .. }
                    | InstructionValue::PropertyLoad { .. }
                    | InstructionValue::StoreLocal { .. }
                ))
                .unwrap_or(true);
            if !is_transparent {
                if std::env::var("RC_DEBUG2").is_ok() {
                    let kind = instr_opt.map(|i| format!("{:?}", std::mem::discriminant(&i.value))).unwrap_or_else(|| "None".to_string());
                    eprintln!("[count_outputs] scope {:?} counting lv_id={} ({}) as output", sid.0, lv_id, kind);
                }
                n_ssa_outputs += 1;
            }
        }
        result.insert(sid, n_ssa_outputs.max(1));
    }
    result
}

impl<'a> Codegen<'a> {
    fn new(hir: &'a HIRFunction, env: &'a Environment) -> Self {
        // Pre-compute the number of escaping StoreLocal outputs per scope.
        // Each escaping named variable needs its own cache slot.
        let scope_output_counts = count_scope_outputs(hir, env);

        // Assign slots to scopes in order of their range start.
        // Scopes with N dependencies and M outputs get N+M slots.
        let mut scopes_sorted: Vec<(&ScopeId, &ReactiveScope)> = env.scopes.iter().collect();
        scopes_sorted.sort_by_key(|(_, s)| s.range.start.0);

        let mut dep_slots: HashMap<ScopeId, Vec<usize>> = HashMap::new();
        let mut output_slots: HashMap<ScopeId, Vec<usize>> = HashMap::new();
        let mut current_offset = 0usize;

        for (sid, scope) in &scopes_sorted {
            let n_deps = scope.dependencies.len();
            let n_outputs = scope_output_counts.get(*sid).copied().unwrap_or(1);
            let mut deps_vec = Vec::with_capacity(n_deps);
            for i in 0..n_deps {
                deps_vec.push(current_offset + i);
            }
            dep_slots.insert(**sid, deps_vec);
            let mut out_vec = Vec::with_capacity(n_outputs);
            for i in 0..n_outputs {
                out_vec.push(current_offset + n_deps + i);
            }
            output_slots.insert(**sid, out_vec);
            current_offset += n_deps + n_outputs;
        }
        let num_scopes = current_offset;

        // Build instruction map and use counts.
        let mut instr_map = HashMap::new();
        let mut use_count: HashMap<u32, u32> = HashMap::new();

        for (_, block) in &hir.body.blocks {
            for instr in &block.instructions {
                instr_map.insert(instr.lvalue.identifier.0, instr.clone());
                for id in collect_instr_operands(instr) {
                    *use_count.entry(id.0).or_insert(0) += 1;
                }
            }
            // Count terminal operands too.
            use crate::hir::visitors::each_terminal_operand;
            for op in each_terminal_operand(&block.terminal) {
                *use_count.entry(op.identifier.0).or_insert(0) += 1;
            }
        }

        // Count how many function params were promoted to tN names.
        // Scope result temps in codegen start from this offset to avoid collision.
        let param_name_offset = hir.params.iter().filter(|p| {
            let id = match p {
                crate::hir::hir::Param::Place(pl) => pl.identifier,
                crate::hir::hir::Param::Spread(s) => s.place.identifier,
            };
            matches!(
                env.get_identifier(id).and_then(|i| i.name.as_ref()),
                Some(crate::hir::hir::IdentifierName::Promoted(_))
            )
        }).count();

        // Build reverse map: SSA temp id → name of named variable assigned from it.
        // e.g. `const a = $t31` → ssa_value_to_name[31] = "a"
        let mut ssa_value_to_name: HashMap<u32, String> = HashMap::new();
        for (_, block) in &hir.body.blocks {
            for instr in &block.instructions {
                if let InstructionValue::StoreLocal { lvalue, value, .. } = &instr.value {
                    // lvalue has a name; value may be an unnamed SSA temp
                    let lvalue_name = env.get_identifier(lvalue.place.identifier)
                        .and_then(|i| i.name.as_ref())
                        .map(|n| n.value().to_string());
                    let value_has_name = env.get_identifier(value.identifier)
                        .and_then(|i| i.name.as_ref())
                        .is_some();
                    if let Some(name) = lvalue_name {
                        if !value_has_name {
                            ssa_value_to_name.insert(value.identifier.0, name);
                        }
                    }
                }
            }
        }

        // Build instr_to_block map.
        let mut instr_to_block: HashMap<InstructionId, BlockId> = HashMap::new();
        for (&bid, block) in &hir.body.blocks {
            for instr in &block.instructions {
                instr_to_block.insert(instr.id, bid);
            }
        }

        // Pre-compute which named variable IDs appear in any InlineJs source string.
        // InlineJs instructions reference variables by name (not by HIR Place operands),
        // so we scan their source text against all named StoreLocal variables.
        let mut inline_js_referenced_ids = std::collections::HashSet::new();
        let mut inline_js_sources: Vec<String> = Vec::new();
        for (_, block) in &hir.body.blocks {
            for instr in &block.instructions {
                if let InstructionValue::InlineJs { source, .. } = &instr.value {
                    inline_js_sources.push(source.clone());
                }
            }
        }
        if !inline_js_sources.is_empty() {
            let combined = inline_js_sources.join(" ");
            for (_, block) in &hir.body.blocks {
                for instr in &block.instructions {
                    if let InstructionValue::StoreLocal { lvalue, .. } = &instr.value {
                        if let Some(name) = env.get_identifier(lvalue.place.identifier)
                            .and_then(|i| i.name.as_ref())
                            .map(|n| n.value().to_string())
                        {
                            if contains_as_word_codegen(&combined, &name) {
                                inline_js_referenced_ids.insert(lvalue.place.identifier);
                            }
                        }
                    }
                }
            }
        }

        // --- Variable shadowing detection ---
        // When two declarations share the same source name (shadowing), rename the
        // later ones to "name_0", "name_1", etc.
        // Priority order: function params always keep their name; among body variables,
        // the first-seen (lowest InstructionId order) keeps the name.
        // This mirrors the TS compiler's rename_variables pass.
        let mut name_overrides: HashMap<u32, String> = HashMap::new();
        {
            // Collect param names as permanently taken.
            let mut param_ids: std::collections::HashSet<u32> = std::collections::HashSet::new();
            let mut taken_names: HashMap<String, u32> = HashMap::new(); // name → id that owns it
            for param in &hir.params {
                let id = match param {
                    crate::hir::hir::Param::Place(pl) => pl.identifier,
                    crate::hir::hir::Param::Spread(s) => s.place.identifier,
                };
                param_ids.insert(id.0);
                if let Some(name) = env.get_identifier(id).and_then(|i| i.name.as_ref()) {
                    taken_names.insert(name.value().to_string(), id.0);
                }
            }

            // Walk all StoreLocal/DeclareLocal in instruction-id order.
            // Build (instr_id, var_id) pairs to sort by instr_id.
            let mut decl_order: Vec<(u32, u32)> = Vec::new(); // (instr_id, var_id)
            let mut seen_var_ids: std::collections::HashSet<u32> = std::collections::HashSet::new();
            for (_, block) in &hir.body.blocks {
                for instr in &block.instructions {
                    let vid_opt: Option<u32> = match &instr.value {
                        InstructionValue::StoreLocal { lvalue, .. }
                        | InstructionValue::DeclareLocal { lvalue, .. } => {
                            let vid = lvalue.place.identifier.0;
                            if seen_var_ids.insert(vid) { Some(vid) } else { None }
                        }
                        _ => None,
                    };
                    if let Some(vid) = vid_opt {
                        decl_order.push((instr.id.0, vid));
                    }
                }
            }
            decl_order.sort_by_key(|(iid, _)| *iid);
            if std::env::var("RC_DEBUG_RENAME").is_ok() {
                eprintln!("[rename] decl_order len={} taken_names len={}", decl_order.len(), taken_names.len());
                for (iid, vid) in &decl_order {
                    let id = crate::hir::hir::IdentifierId(*vid);
                    let name = env.get_identifier(id).and_then(|i| i.name.as_ref()).map(|n| n.value().to_string()).unwrap_or_else(|| "?".to_string());
                    eprintln!("[rename]   iid={} vid={} name={}", iid, vid, name);
                }
            }

            // Track how many times each name has been disambiguated.
            let mut name_suffix_count: HashMap<String, u32> = HashMap::new();

            for (iid, vid) in &decl_order {
                let id = crate::hir::hir::IdentifierId(*vid);
                if let Some(name) = env.get_identifier(id).and_then(|i| i.name.as_ref()) {
                    let base = name.value().to_string();
                    if std::env::var("RC_DEBUG_RENAME").is_ok() {
                        eprintln!("[rename] instr_id={} var_id={} name={} taken={}", iid, vid, base, taken_names.contains_key(&base));
                    }
                    if taken_names.contains_key(&base) {
                        // Collision: rename this variable.
                        let suffix = name_suffix_count.entry(base.clone()).or_insert(0);
                        let new_name = format!("{}_{}", base, *suffix);
                        *suffix += 1;
                        name_overrides.insert(*vid, new_name.clone());
                        // Also update ssa_value_to_name for the StoreLocal value id.
                        for (_, block) in &hir.body.blocks {
                            for instr in &block.instructions {
                                if let InstructionValue::StoreLocal { lvalue, value, .. } = &instr.value {
                                    if lvalue.place.identifier.0 == *vid {
                                        let val_id = value.identifier.0;
                                        if env.get_identifier(value.identifier).and_then(|i| i.name.as_ref()).is_none() {
                                            ssa_value_to_name.insert(val_id, new_name.clone());
                                        }
                                    }
                                }
                            }
                        }
                    } else {
                        // First occurrence of this name: take it.
                        taken_names.insert(base, *vid);
                    }
                }
            }
        }

        // Precompute DeclarationIds of variables that are reassigned or mutated via
        // update expressions. Destructuring bindings with these IDs should use `let`.
        let mut reassigned_decl_ids = std::collections::HashSet::new();
        for (_, block) in &hir.body.blocks {
            for instr in &block.instructions {
                match &instr.value {
                    InstructionValue::StoreLocal { lvalue, .. }
                        if matches!(lvalue.kind, InstructionKind::Reassign) =>
                    {
                        if let Some(ident) = env.get_identifier(lvalue.place.identifier) {
                            reassigned_decl_ids.insert(ident.declaration_id);
                        }
                    }
                    InstructionValue::PrefixUpdate { lvalue, .. }
                    | InstructionValue::PostfixUpdate { lvalue, .. } => {
                        if let Some(ident) = env.get_identifier(lvalue.identifier) {
                            reassigned_decl_ids.insert(ident.declaration_id);
                        }
                    }
                    _ => {}
                }
            }
        }

        Codegen {
            hir,
            env,
            inlined_exprs: HashMap::new(),
            dep_slots,
            output_slots,
            num_scopes,
            param_name_offset,
            instr_map,
            use_count,
            terminal_replacement: HashMap::new(),
            ssa_value_to_name,
            instr_to_block,
            within_loop_scopes: std::collections::HashSet::new(),
            inline_js_referenced_ids,
            name_overrides,
            switch_labels: HashMap::new(),
            switch_fallthrough_labels: HashMap::new(),
            scope_output_names: HashMap::new(),
            reassigned_decl_ids,
            label_fallthrough_blocks: std::collections::HashSet::new(),
        }
    }

    fn emit(&mut self) -> String {
        // Assign sequential label numbers to switches that need labels.
        // A switch needs a label when any block in any case subgraph has an
        // explicit Break goto to the fallthrough (i.e., source had `break;`).
        let mut label_counter: u32 = 0;
        for (_, block) in &self.hir.body.blocks {
            if let Terminal::Switch { cases, fallthrough, id: switch_id, .. } = &block.terminal {
                let fall_bid = *fallthrough;
                let needs_label = cases.iter().any(|c| {
                    case_subgraph_has_explicit_break(&self.hir.body.blocks, c.block, fall_bid)
                });
                if needs_label {
                    let label_str = format!("bb{label_counter}");
                    self.switch_labels.insert(*switch_id, label_counter);
                    self.switch_fallthrough_labels.insert(fall_bid, label_str);
                    label_counter += 1;
                }
            }
            // Also assign labels to Label terminals whose body has a Break to fallthrough.
            if let Terminal::Label { block: body, fallthrough, .. } = &block.terminal {
                let fall_bid = *fallthrough;
                if case_subgraph_has_explicit_break(&self.hir.body.blocks, *body, fall_bid) {
                    let label_str = format!("bb{label_counter}");
                    self.switch_fallthrough_labels.insert(fall_bid, label_str);
                    self.label_fallthrough_blocks.insert(fall_bid);
                    label_counter += 1;
                }
            }
        }

        // Collect instructions in block order.
        let ordered = self.collect_instructions_in_order();

        // Build inlined_exprs for transparent single-use temps.
        self.build_inline_map(&ordered);

        // Determine which scope each instruction belongs to.
        let instr_scope = self.assign_instructions_to_scopes(&ordered);

        // Build "should_inline" set: instructions that are fully inlined
        // and should NOT produce standalone statements.
        let inlined_ids = self.collect_inlined_ids(&ordered);

        let mut out = String::new();

        let fn_name = self.hir.id.as_deref().unwrap_or("anonymous");
        let async_kw = if self.hir.async_ { "async " } else { "" };
        let params = self.emit_params();

        // Only emit the runtime import when there are actual cache slots.
        if self.num_scopes > 0 {
            let _ = writeln!(out, "import {{ c as _c }} from \"react/compiler-runtime\";");
        }
        if self.hir.is_arrow {
            // Arrow function form: `const Name = (params) => { ... };`
            let export_kw = if self.hir.is_default_export { "export default " }
                else if self.hir.is_named_export { "export " }
                else { "" };
            let _ = writeln!(out, "{export_kw}const {fn_name} = {async_kw}({params}) => {{");
        } else if self.hir.is_default_export {
            let _ = writeln!(out, "export default {async_kw}function {fn_name}({params}) {{");
        } else if self.hir.is_named_export {
            let _ = writeln!(out, "export {async_kw}function {fn_name}({params}) {{");
        } else {
            let _ = writeln!(out, "{async_kw}function {fn_name}({params}) {{");
        }

        // Emit non-opt-out function-body directives (e.g., "use foo"; "use bar";).
        for directive in &self.hir.directives {
            let _ = writeln!(out, "  \"{directive}\";");
        }

        if self.num_scopes > 0 {
            let _ = writeln!(out, "  const $ = _c({});", self.num_scopes);
        }

        if std::env::var("RC_DEBUG").is_ok() {
            eprintln!("[codegen] {} scopes, {} instrs", self.num_scopes, ordered.len());
            let mut scope_vec: Vec<_> = self.env.scopes.iter().collect();
            scope_vec.sort_by_key(|(_, s)| s.range.start.0);
            for (sid, scope) in &scope_vec {
                let named_decls: Vec<String> = scope.declarations.keys()
                    .filter_map(|&id| self.env.get_identifier(id).and_then(|i| i.name.as_ref()).map(|n| format!("{}({})", id.0, n.value())))
                    .collect();
                let all_decl_ids: Vec<u32> = scope.declarations.keys().map(|id| id.0).collect();
                eprintln!("  scope {:?} range=[{},{}] named_decls={:?} all_decl_ids={:?}", sid.0, scope.range.start.0, scope.range.end.0, named_decls, all_decl_ids);
            }
            for instr in &ordered {
                let scope_info = instr_scope.get(&instr.id).map(|s| format!("scope={}", s.0));
                eprintln!("  instr[{}] {:?} lv=$t{}{}", instr.id.0,
                    std::mem::discriminant(&instr.value),
                    instr.lvalue.identifier.0,
                    scope_info.map(|s| format!(" {s}")).unwrap_or_default());
            }
        }

        // Pre-compute scope_instrs: ScopeId → flat list of Instruction refs (owned clones).
        // This mirrors the grouped analysis but stored by scope for emit_scope_block_inner.
        let grouped = group_by_scope(&ordered, &instr_scope, &self.inlined_exprs);
        let mut scope_instrs_map: HashMap<ScopeId, Vec<Instruction>> = HashMap::new();
        for group in &grouped {
            if let InstrGroup::Scoped(sid, instrs) = group {
                scope_instrs_map.entry(*sid)
                    .or_default()
                    .extend(instrs.iter().map(|i| (*i).clone()));
            }
        }

        // scope_index counts scopes in emission order (0-based) for temp naming.
        let mut scope_index: usize = 0;
        let mut emitted_scopes: std::collections::HashSet<ScopeId> = std::collections::HashSet::new();
        let mut visited: std::collections::HashSet<BlockId> = std::collections::HashSet::new();
        let mut inlined_ids_mut = inlined_ids.clone();

        self.emit_cfg_region(
            self.hir.body.entry,
            None,
            1,
            &mut out,
            &mut visited,
            &mut emitted_scopes,
            &mut scope_index,
            &instr_scope,
            &mut inlined_ids_mut,
            &scope_instrs_map,
        );

        if self.hir.is_arrow {
            let _ = writeln!(out, "}};");
        } else {
            let _ = writeln!(out, "}}");
        }
        out
    }

    // -----------------------------------------------------------------------
    // CFG-recursive emission
    // -----------------------------------------------------------------------

    /// Recursively walk the CFG starting at `start`, emitting statements to `out`.
    /// Stops before emitting `stop_at` (exclusive). `indent` controls indentation level
    /// (1 = two spaces per level → 2 spaces for top-level function body).
    #[allow(clippy::too_many_arguments)]
    fn emit_cfg_region(
        &mut self,
        start: BlockId,
        stop_at: Option<BlockId>,
        indent: usize,
        out: &mut String,
        visited: &mut std::collections::HashSet<BlockId>,
        emitted_scopes: &mut std::collections::HashSet<ScopeId>,
        scope_index: &mut usize,
        instr_scope: &HashMap<InstructionId, ScopeId>,
        inlined_ids: &mut std::collections::HashSet<u32>,
        scope_instrs: &HashMap<ScopeId, Vec<Instruction>>,
    ) {
        let pad = "  ".repeat(indent);
        let mut current = start;

        loop {
            // Stop if we've reached the stop block.
            if Some(current) == stop_at {
                return;
            }
            // Avoid infinite loops / re-visiting (e.g. loop back-edges).
            if !visited.insert(current) {
                return;
            }

            let block = match self.hir.body.blocks.get(&current).cloned() {
                Some(b) => b,
                None => return,
            };

            // Pre-check: if the block's terminal is a loop terminal, find any
            // unvisited scope whose instructions span into the loop body.
            // We defer those scopes' emission to the loop-wrapping handler below
            // instead of emitting them flat here.
            let deferred_loop_scope: Option<(ScopeId, BlockId, BlockId, BlockId, bool)> =
                match &block.terminal {
                    Terminal::DoWhile { loop_, test, fallthrough, .. } => {
                        let loop_bid = *loop_;
                        let test_bid = *test;
                        let fall_bid = *fallthrough;
                        self.find_scope_for_loop_body(loop_bid, instr_scope, emitted_scopes)
                            .map(|sid| (sid, loop_bid, test_bid, fall_bid, false /* is_while */))
                    }
                    Terminal::While { test, loop_, fallthrough, .. } => {
                        let loop_bid = *loop_;
                        let test_bid = *test;
                        let fall_bid = *fallthrough;
                        self.find_scope_for_loop_body(loop_bid, instr_scope, emitted_scopes)
                            .map(|sid| (sid, loop_bid, test_bid, fall_bid, true /* is_while */))
                    }
                    // For ForOf/ForIn: also defer scope to avoid premature flat emission.
                    // The terminal handler at line 1074+ will wrap the loop with the scope.
                    Terminal::ForOf { loop_, test, fallthrough, .. } => {
                        let loop_bid = *loop_;
                        let test_bid = *test;
                        let fall_bid = *fallthrough;
                        self.find_scope_for_loop_body(loop_bid, instr_scope, emitted_scopes)
                            .map(|sid| (sid, loop_bid, test_bid, fall_bid, true /* use is_while=true as placeholder */))
                    }
                    Terminal::ForIn { loop_, fallthrough, .. } => {
                        let loop_bid = *loop_;
                        // ForIn has no test block; use loop_bid as test_bid placeholder
                        let fall_bid = *fallthrough;
                        self.find_scope_for_loop_body(loop_bid, instr_scope, emitted_scopes)
                            .map(|sid| (sid, loop_bid, loop_bid, fall_bid, true))
                    }
                    _ => None,
                };
            // Scopes that should be emitted via loop-wrapping (not flat).
            let deferred_sid: Option<ScopeId> = deferred_loop_scope.map(|(sid, ..)| sid);

            // For ForIn/ForOf terminals: the init block (== current block) contains
            // instructions whose lvalues are consumed by the terminal handler, not
            // emitted as standalone statements.  Pre-mark them as inlined so the
            // instruction emission loop below skips them.
            match &block.terminal {
                Terminal::ForIn { .. } => {
                    for instr in &block.instructions {
                        if let InstructionValue::NextPropertyOf { .. } = &instr.value {
                            inlined_ids.insert(instr.lvalue.identifier.0);
                        }
                    }
                }
                Terminal::ForOf { .. } => {
                    for instr in &block.instructions {
                        if let InstructionValue::GetIterator { .. } = &instr.value {
                            inlined_ids.insert(instr.lvalue.identifier.0);
                        }
                    }
                }
                Terminal::For { .. } => {
                    // For-loop init instructions are at the END of the current block
                    // (emitted by lower_for before the For terminal). Walk backwards
                    // from the end and collect DeclareLocal/StoreLocal pairs that
                    // form `let i = 0` style init declarations.
                    for instr in block.instructions.iter().rev() {
                        match &instr.value {
                            InstructionValue::DeclareLocal { .. }
                            | InstructionValue::StoreLocal { .. } => {
                                inlined_ids.insert(instr.lvalue.identifier.0);
                            }
                            _ => break, // Stop at first non-decl/store instruction
                        }
                    }
                }
                _ => {}
            }

            // Emit instructions in this block.
            for instr in &block.instructions {
                let lv_id = instr.lvalue.identifier.0;

                // Check if this instruction belongs to a scope.
                if let Some(&sid) = instr_scope.get(&instr.id) {
                    if self.within_loop_scopes.contains(&sid) {
                        // We're inside this scope's "new branch" via loop-wrapping.
                        // Emit the instruction as a plain statement.
                        if !inlined_ids.contains(&lv_id) {
                            if let Some(s) = self.emit_stmt(instr, None, &[]) {
                                for line in s.lines() {
                                    let _ = writeln!(out, "{pad}{}", line);
                                }
                            }
                        }
                        continue;
                    }
                    if deferred_sid == Some(sid) {
                        // This scope will be emitted as a loop-wrapped block below.
                        // Skip flat emission here.
                        continue;
                    }
                    if !emitted_scopes.contains(&sid) {
                        // Emit the whole scope block now.
                        let scope_instrs_list = scope_instrs.get(&sid).cloned().unwrap_or_default();
                        let scope_instr_refs: Vec<&Instruction> = scope_instrs_list.iter().collect();
                        self.emit_scope_block_inner(
                            &sid,
                            &scope_instr_refs,
                            indent,
                            scope_index,
                            out,
                            inlined_ids,
                        );
                        emitted_scopes.insert(sid);
                    }
                    continue;
                }

                // Not in a scope — emit directly.
                if inlined_ids.contains(&lv_id) {
                    continue;
                }
                if let Some(s) = self.emit_stmt(instr, None, &[]) {
                    for line in s.lines() {
                        let _ = writeln!(out, "{pad}{}", line);
                    }
                }
            }

            // Handle terminal.
            match &block.terminal.clone() {
                Terminal::Return { .. } | Terminal::Throw { .. } => {
                    let js = self.emit_terminal(&block.terminal);
                    if !js.is_empty() {
                        for line in js.lines() {
                            let _ = writeln!(out, "{pad}{}", line);
                        }
                    }
                    return;
                }

                Terminal::Goto { block: next, variant, .. } => {
                    match variant {
                        GotoVariant::Break => {
                            if Some(*next) == stop_at {
                                // Exiting the current control structure — natural fallthrough.
                                // For Label terminal fallthroughs, this is natural exit (no break needed).
                                // For switch fallthroughs, we still need `break <label>;`.
                                if !self.label_fallthrough_blocks.contains(next) {
                                    if let Some(label) = self.switch_fallthrough_labels.get(next).cloned() {
                                        let _ = writeln!(out, "{pad}break {label};");
                                    }
                                }
                                return;
                            } else if stop_at.is_some() {
                                // Breaking out of an inner structure (loop inside switch, etc.)
                                if let Some(label) = self.switch_fallthrough_labels.get(next).cloned() {
                                    let _ = writeln!(out, "{pad}break {label};");
                                } else {
                                    let _ = writeln!(out, "{pad}break;");
                                }
                                return;
                            }
                            current = *next;
                            continue;
                        }
                        GotoVariant::Continue => {
                            // Emit `continue;` when target is an already-visited loop
                            // header, but NOT when it's just the natural end of the
                            // loop body (stop_at == target).
                            if visited.contains(next) && stop_at.map_or(false, |s| s != *next) {
                                let _ = writeln!(out, "{pad}continue;");
                                return;
                            }
                            current = *next;
                            continue;
                        }
                        GotoVariant::Try => {
                            current = *next;
                            continue;
                        }
                    }
                }

                Terminal::If { test, consequent, alternate, fallthrough, .. } => {
                    let test_expr = self.expr(test);
                    let _ = writeln!(out, "{pad}if ({test_expr}) {{");
                    let body_pad = indent + 1;
                    let mut vis2 = visited.clone();
                    self.emit_cfg_region(
                        *consequent, Some(*fallthrough), body_pad, out,
                        &mut vis2, emitted_scopes, scope_index, instr_scope, inlined_ids, scope_instrs,
                    );
                    // Merge visited from consequent walk (but not alternate).
                    // Only mark consequent visited so alternate isn't skipped.
                    let emit_else = *alternate != *fallthrough;
                    if emit_else {
                        // Emit else body to temp buffer; skip if empty.
                        let mut else_buf = String::new();
                        let mut vis3 = visited.clone();
                        self.emit_cfg_region(
                            *alternate, Some(*fallthrough), body_pad, &mut else_buf,
                            &mut vis3, emitted_scopes, scope_index, instr_scope, inlined_ids, scope_instrs,
                        );
                        if !else_buf.trim().is_empty() {
                            let _ = writeln!(out, "{pad}}} else {{");
                            out.push_str(&else_buf);
                        }
                    }
                    let _ = writeln!(out, "{pad}}}");
                    // Continue at fallthrough.
                    if Some(*fallthrough) == stop_at {
                        return;
                    }
                    current = *fallthrough;
                    continue;
                }

                Terminal::Branch { fallthrough, .. } => {
                    // Branch terminals appear in while/for test blocks.
                    // The caller handles the loop structure; just follow fallthrough.
                    if Some(*fallthrough) == stop_at {
                        return;
                    }
                    current = *fallthrough;
                    continue;
                }

                Terminal::While { test, loop_, fallthrough, .. } => {
                    let test_bid = *test;
                    let loop_bid = *loop_;
                    let fall_bid = *fallthrough;

                    // Check if loop body contains unvisited scope instructions (pre-detected).
                    if let Some((sid, _, _, _, true)) = deferred_loop_scope {
                        // Collect pre-loop scope instructions.
                        let scope_instrs_list = scope_instrs.get(&sid).cloned().unwrap_or_default();
                        let pre_loop = self.collect_pre_loop_scope_instrs(&scope_instrs_list, loop_bid, Some(test_bid));
                        self.emit_loop_wrapped_scope(
                            sid, &pre_loop, loop_bid, test_bid, fall_bid,
                            LoopType::While { test_bid },
                            indent, scope_index, out, visited, emitted_scopes,
                            instr_scope, inlined_ids, scope_instrs,
                        );
                        if Some(fall_bid) == stop_at { return; }
                        current = fall_bid;
                        continue;
                    }

                    // Normal while loop (no scope wrapping).
                    // Mark test block as visited so the recursive body walk doesn't re-enter it.
                    visited.insert(test_bid);
                    // Collect test expression from the Branch terminal of the test block.
                    let test_expr = self.hir.body.blocks.get(&test_bid).and_then(|b| {
                        if let Terminal::Branch { test, .. } = &b.terminal {
                            Some(self.expr(test))
                        } else {
                            None
                        }
                    }).unwrap_or_else(|| "true".to_string());
                    // Also emit any instructions in the test block (condition computations).
                    let test_block_instrs = self.hir.body.blocks.get(&test_bid)
                        .map(|b| b.instructions.clone())
                        .unwrap_or_default();
                    for instr in &test_block_instrs {
                        if inlined_ids.contains(&instr.lvalue.identifier.0) { continue; }
                        // Skip test-expression instructions — they're inlined into the while condition.
                        if self.inlined_exprs.contains_key(&instr.lvalue.identifier.0) { continue; }
                        if let Some(s) = self.emit_stmt(instr, None, &[]) {
                            for line in s.lines() {
                                let _ = writeln!(out, "{pad}{}", line);
                            }
                        }
                    }
                    let _ = writeln!(out, "{pad}while ({test_expr}) {{");
                    let body_pad = indent + 1;
                    let mut vis2 = visited.clone();
                    vis2.insert(test_bid);
                    self.emit_cfg_region(
                        loop_bid, Some(test_bid), body_pad, out,
                        &mut vis2, emitted_scopes, scope_index, instr_scope, inlined_ids, scope_instrs,
                    );
                    let _ = writeln!(out, "{pad}}}");
                    if Some(fall_bid) == stop_at {
                        return;
                    }
                    current = fall_bid;
                    continue;
                }

                Terminal::DoWhile { loop_, test, fallthrough, .. } => {
                    let loop_bid = *loop_;
                    let test_bid = *test;
                    let fall_bid = *fallthrough;

                    // Check if loop body contains unvisited scope instructions (pre-detected).
                    if let Some((sid, _, _, _, false)) = deferred_loop_scope {
                        // Collect pre-loop scope instructions.
                        let scope_instrs_list = scope_instrs.get(&sid).cloned().unwrap_or_default();
                        let pre_loop = self.collect_pre_loop_scope_instrs(&scope_instrs_list, loop_bid, Some(test_bid));
                        self.emit_loop_wrapped_scope(
                            sid, &pre_loop, loop_bid, test_bid, fall_bid,
                            LoopType::DoWhile,
                            indent, scope_index, out, visited, emitted_scopes,
                            instr_scope, inlined_ids, scope_instrs,
                        );
                        if Some(fall_bid) == stop_at { return; }
                        current = fall_bid;
                        continue;
                    }

                    // Normal do-while loop (no scope wrapping).
                    visited.insert(test_bid);
                    let test_expr = self.hir.body.blocks.get(&test_bid).and_then(|b| {
                        if let Terminal::Branch { test, .. } = &b.terminal {
                            Some(self.expr(test))
                        } else {
                            None
                        }
                    }).unwrap_or_else(|| "true".to_string());
                    let _ = writeln!(out, "{pad}do {{");
                    let body_pad = indent + 1;
                    let mut vis2 = visited.clone();
                    vis2.insert(test_bid);
                    self.emit_cfg_region(
                        loop_bid, Some(test_bid), body_pad, out,
                        &mut vis2, emitted_scopes, scope_index, instr_scope, inlined_ids, scope_instrs,
                    );
                    let _ = writeln!(out, "{pad}}} while ({test_expr});");
                    if Some(fall_bid) == stop_at {
                        return;
                    }
                    current = fall_bid;
                    continue;
                }

                Terminal::For { init, test, update, loop_, fallthrough, .. } => {
                    let init_bid = *init;
                    let test_bid = *test;
                    let update_bid = *update;
                    let loop_bid = *loop_;
                    let fall_bid = *fallthrough;
                    // Mark test/update as visited so the body walk doesn't enter them.
                    visited.insert(test_bid);
                    if let Some(ubid) = update_bid {
                        visited.insert(ubid);
                    }

                    // Reconstruct init expression from the init block's inlined
                    // instructions (only those pre-marked as inlined above).
                    let init_expr = self.hir.body.blocks.get(&init_bid).map(|b| {
                        let parts: Vec<String> = b.instructions.iter()
                            .filter(|instr| inlined_ids.contains(&instr.lvalue.identifier.0))
                            .filter_map(|instr| {
                                self.emit_stmt(instr, None, &[])
                                    .map(|s| s.trim_end_matches(';').to_string())
                            }).collect();
                        parts.join(", ")
                    }).unwrap_or_default();

                    let test_expr = self.hir.body.blocks.get(&test_bid).and_then(|b| {
                        if let Terminal::Branch { test, .. } = &b.terminal {
                            Some(self.expr(test))
                        } else {
                            None
                        }
                    }).unwrap_or_else(|| "true".to_string());

                    // Reconstruct update expression from the update block.
                    // Find the last emittable instruction (typically StoreLocal i = i + 1).
                    // For Primitive instructions (from const-prop), emit just the value
                    // rather than a declaration.
                    let update_expr = update_bid.and_then(|ubid| {
                        self.hir.body.blocks.get(&ubid).and_then(|b| {
                            let mut last = None;
                            for instr in &b.instructions {
                                // In for-loop update position, use expression form
                                // for Primitives and LoadLocals instead of declarations.
                                match &instr.value {
                                    InstructionValue::Primitive { value, .. } => {
                                        last = Some(primitive_expr(value));
                                    }
                                    InstructionValue::LoadLocal { place, .. } => {
                                        last = Some(self.expr(place));
                                    }
                                    InstructionValue::LoadGlobal { .. } => {
                                        last = Some(self.expr(&instr.lvalue));
                                    }
                                    _ => {
                                        if let Some(s) = self.emit_stmt(instr, None, &[]) {
                                            last = Some(s.trim_end_matches(';').to_string());
                                        }
                                    }
                                }
                            }
                            last
                        })
                    }).unwrap_or_default();

                    let _ = writeln!(out, "{pad}for ({init_expr}; {test_expr}; {update_expr}) {{");
                    let body_pad = indent + 1;
                    let mut vis2 = visited.clone();
                    if let Some(ubid) = update_bid { vis2.insert(ubid); }
                    vis2.insert(test_bid);
                    // stop_at = update block (if present), so the body's implicit
                    // continue to the update block doesn't emit a spurious `continue;`.
                    let body_stop = update_bid.unwrap_or(test_bid);
                    self.emit_cfg_region(
                        loop_bid, Some(body_stop), body_pad, out,
                        &mut vis2, emitted_scopes, scope_index, instr_scope, inlined_ids, scope_instrs,
                    );
                    let _ = writeln!(out, "{pad}}}");
                    if Some(fall_bid) == stop_at {
                        return;
                    }
                    current = fall_bid;
                    continue;
                }

                Terminal::ForOf { init, test, loop_, fallthrough, .. } => {
                    let init_bid = *init;
                    let test_bid = *test;
                    let loop_bid = *loop_;
                    let fall_bid = *fallthrough;

                    // Find the iterable from the init block's GetIterator instruction.
                    // Resolve through the init block's instruction chain since those
                    // instructions may not have been visited yet for inlining.
                    let iterable_expr = self.hir.body.blocks.get(&init_bid).and_then(|b| {
                        // Build a local map of identifier → expression for this block.
                        let mut local_exprs: std::collections::HashMap<u32, String> = std::collections::HashMap::new();
                        for instr in &b.instructions {
                            match &instr.value {
                                InstructionValue::LoadLocal { place, .. } => {
                                    let name = self.ident_name(place.identifier);
                                    local_exprs.insert(instr.lvalue.identifier.0, name);
                                }
                                InstructionValue::PropertyLoad { object, property, .. } => {
                                    let obj = local_exprs.get(&object.identifier.0)
                                        .cloned()
                                        .unwrap_or_else(|| self.expr(object));
                                    local_exprs.insert(instr.lvalue.identifier.0, format!("{}.{}", obj, property));
                                }
                                InstructionValue::ComputedLoad { object, property, .. } => {
                                    let obj = local_exprs.get(&object.identifier.0)
                                        .cloned()
                                        .unwrap_or_else(|| self.expr(object));
                                    let prop = local_exprs.get(&property.identifier.0)
                                        .cloned()
                                        .unwrap_or_else(|| self.expr(property));
                                    local_exprs.insert(instr.lvalue.identifier.0, format!("{}[{}]", obj, prop));
                                }
                                InstructionValue::GetIterator { collection, .. } => {
                                    let coll = local_exprs.get(&collection.identifier.0)
                                        .cloned()
                                        .unwrap_or_else(|| self.expr(collection));
                                    return Some(coll);
                                }
                                _ => {}
                            }
                        }
                        None
                    }).unwrap_or_else(|| "undefined".to_string());

                    // Find the loop variable from the loop_ block's first StoreLocal.
                    // Also check instr.lvalue.identifier (post-SSA named version).
                    if std::env::var("RC_DEBUG").is_ok() {
                        if let Some(b) = self.hir.body.blocks.get(&loop_bid) {
                            eprintln!("[for-of debug] loop_bid={:?} instrs={}", loop_bid, b.instructions.len());
                            for instr in &b.instructions {
                                eprintln!("  instr[{}] kind={:?} lv_id={}", instr.id.0, std::mem::discriminant(&instr.value), instr.lvalue.identifier.0);
                                if let InstructionValue::StoreLocal { lvalue, .. } = &instr.value {
                                    let n1 = self.env.get_identifier(lvalue.place.identifier).and_then(|i| i.name.as_ref()).map(|n| n.value().to_string());
                                    let n2 = self.env.get_identifier(instr.lvalue.identifier).and_then(|i| i.name.as_ref()).map(|n| n.value().to_string());
                                    eprintln!("    StoreLocal lvalue.place.id={} name={:?} | instr.lv.id={} name={:?}", lvalue.place.identifier.0, n1, instr.lvalue.identifier.0, n2);
                                }
                            }
                        } else {
                            eprintln!("[for-of debug] loop_bid={:?} NOT FOUND in blocks", loop_bid);
                        }
                    }
                    let (loop_var_name, binding_id) = self.hir.body.blocks.get(&loop_bid).and_then(|b| {
                        b.instructions.iter().find_map(|instr| {
                            if let InstructionValue::StoreLocal { lvalue, .. } = &instr.value {
                                // Try the inner lvalue.place (pre-SSA original identifier).
                                let name = self.env.get_identifier(lvalue.place.identifier)
                                    .and_then(|i| i.name.as_ref())
                                    .map(|n| n.value().to_string())
                                    // Fallback: try the instruction's lvalue (post-SSA identifier).
                                    .or_else(|| self.env.get_identifier(instr.lvalue.identifier)
                                        .and_then(|i| i.name.as_ref())
                                        .map(|n| n.value().to_string()));
                                name.map(|n| (n, instr.lvalue.identifier.0))
                            } else {
                                None
                            }
                        })
                    }).unwrap_or_else(|| ("_item".to_string(), 0));

                    // Check if loop body contains unvisited scope instructions.
                    // If so, wrap the entire loop in a scope block.
                    if let Some(sid) = self.find_scope_for_loop_body(loop_bid, instr_scope, emitted_scopes) {
                        let scope_instrs_list = scope_instrs.get(&sid).cloned().unwrap_or_default();
                        let pre_loop = self.collect_pre_loop_scope_instrs(&scope_instrs_list, loop_bid, Some(test_bid));
                        self.emit_loop_wrapped_scope(
                            sid, &pre_loop, loop_bid, test_bid, fall_bid,
                            LoopType::ForOf { test_bid, iterable_expr, loop_var_name, binding_id },
                            indent, scope_index, out, visited, emitted_scopes,
                            instr_scope, inlined_ids, scope_instrs,
                        );
                        if Some(fall_bid) == stop_at { return; }
                        current = fall_bid;
                        continue;
                    }

                    // Find the IteratorNext result from the test block.
                    let iter_next_id = self.hir.body.blocks.get(&test_bid).and_then(|b| {
                        b.instructions.iter().rev().find_map(|instr| {
                            if matches!(&instr.value, InstructionValue::IteratorNext { .. }) {
                                Some(instr.lvalue.identifier.0)
                            } else {
                                None
                            }
                        })
                    });

                    // Map the IteratorNext result → loop var name so inner instructions
                    // that load from it resolve correctly.
                    if let Some(iter_id) = iter_next_id {
                        self.inlined_exprs.insert(iter_id, loop_var_name.clone());
                    }

                    // Mark the binding StoreLocal's lvalue as inlined so it's not re-emitted.
                    if binding_id != 0 {
                        inlined_ids.insert(binding_id);
                    }

                    visited.insert(test_bid);

                    let _ = writeln!(out, "{pad}for (const {loop_var_name} of {iterable_expr}) {{");
                    let body_pad = indent + 1;
                    let mut vis2 = visited.clone();
                    vis2.insert(test_bid);
                    self.emit_cfg_region(
                        loop_bid, Some(test_bid), body_pad, out,
                        &mut vis2, emitted_scopes, scope_index, instr_scope, inlined_ids, scope_instrs,
                    );
                    let _ = writeln!(out, "{pad}}}");

                    if Some(fall_bid) == stop_at {
                        return;
                    }
                    current = fall_bid;
                    continue;
                }

                Terminal::ForIn { init, loop_, fallthrough, .. } => {
                    let init_bid = *init;
                    let loop_bid = *loop_;
                    let fall_bid = *fallthrough;

                    // Find the object being iterated from the init block's NextPropertyOf instruction.
                    let object_expr = self.hir.body.blocks.get(&init_bid).and_then(|b| {
                        b.instructions.iter().find_map(|instr| {
                            if let InstructionValue::NextPropertyOf { value, .. } = &instr.value {
                                Some(self.expr(value))
                            } else {
                                None
                            }
                        })
                    }).unwrap_or_else(|| "undefined".to_string());

                    // Find the loop variable from the loop_ block's first StoreLocal.
                    let (loop_var_name, binding_id) = self.hir.body.blocks.get(&loop_bid).and_then(|b| {
                        b.instructions.iter().find_map(|instr| {
                            if let InstructionValue::StoreLocal { lvalue, .. } = &instr.value {
                                let name = self.env.get_identifier(lvalue.place.identifier)
                                    .and_then(|i| i.name.as_ref())
                                    .map(|n| n.value().to_string());
                                name.map(|n| (n, instr.lvalue.identifier.0))
                            } else {
                                None
                            }
                        })
                    }).unwrap_or_else(|| ("_key".to_string(), 0));

                    if binding_id != 0 {
                        inlined_ids.insert(binding_id);
                    }

                    let _ = writeln!(out, "{pad}for (const {loop_var_name} in {object_expr}) {{");
                    let body_pad = indent + 1;
                    let mut vis2 = visited.clone();
                    self.emit_cfg_region(
                        loop_bid, None, body_pad, out,
                        &mut vis2, emitted_scopes, scope_index, instr_scope, inlined_ids, scope_instrs,
                    );
                    let _ = writeln!(out, "{pad}}}");

                    if Some(fall_bid) == stop_at {
                        return;
                    }
                    current = fall_bid;
                    continue;
                }

                Terminal::Try { block, handler, handler_binding, fallthrough, .. } => {
                    let block_bid = *block;
                    let handler_bid = *handler;
                    let fall_bid = *fallthrough;
                    let binding_name = handler_binding.as_ref().map(|p| self.lvalue_name(p));
                    let _ = writeln!(out, "{pad}try {{");
                    let body_pad = indent + 1;
                    let mut vis2 = visited.clone();
                    self.emit_cfg_region(
                        block_bid, Some(fall_bid), body_pad, out,
                        &mut vis2, emitted_scopes, scope_index, instr_scope, inlined_ids, scope_instrs,
                    );
                    if let Some(bname) = binding_name {
                        let _ = writeln!(out, "{pad}}} catch ({bname}) {{");
                    } else {
                        let _ = writeln!(out, "{pad}}} catch (_e) {{");
                    }
                    let mut vis3 = visited.clone();
                    self.emit_cfg_region(
                        handler_bid, Some(fall_bid), body_pad, out,
                        &mut vis3, emitted_scopes, scope_index, instr_scope, inlined_ids, scope_instrs,
                    );
                    let _ = writeln!(out, "{pad}}}");
                    if Some(fall_bid) == stop_at {
                        return;
                    }
                    current = fall_bid;
                    continue;
                }

                Terminal::Switch { test, cases, fallthrough, id: switch_id, .. } => {
                    let test_expr = self.expr(test);
                    let fall_bid = *fallthrough;
                    let switch_label_opt = self.switch_labels.get(switch_id).copied();
                    let switch_label = switch_label_opt.map(|n| format!("bb{n}"));
                    if let Some(ref label) = switch_label {
                        let _ = writeln!(out, "{pad}{label}: switch ({test_expr}) {{");
                    } else {
                        let _ = writeln!(out, "{pad}switch ({test_expr}) {{");
                    }
                    let body_pad = indent + 1;
                    let case_pad = "  ".repeat(body_pad);
                    let inner_pad = "  ".repeat(body_pad + 1);
                    for case in cases {
                        if let Some(t) = &case.test {
                            let _ = writeln!(out, "{case_pad}case {}: {{", self.expr(t));
                        } else {
                            let _ = writeln!(out, "{case_pad}default: {{");
                        }
                        let mut vis2 = visited.clone();
                        self.emit_cfg_region(
                            case.block, Some(fall_bid), body_pad + 1, out,
                            &mut vis2, emitted_scopes, scope_index, instr_scope, inlined_ids, scope_instrs,
                        );
                        let _ = writeln!(out, "{case_pad}}}");
                    }
                    let _ = writeln!(out, "{pad}}}");
                    if Some(fall_bid) == stop_at {
                        return;
                    }
                    current = fall_bid;
                    continue;
                }

                Terminal::Label { block: body, fallthrough, .. } => {
                    let body_bid = *body;
                    let fall_bid = *fallthrough;
                    let label_opt = self.switch_fallthrough_labels.get(&fall_bid).cloned();
                    if let Some(ref label) = label_opt {
                        let _ = writeln!(out, "{pad}{label}: {{");
                        let body_pad = indent + 1;
                        let mut vis2 = visited.clone();
                        self.emit_cfg_region(
                            body_bid, Some(fall_bid), body_pad, out,
                            &mut vis2, emitted_scopes, scope_index, instr_scope, inlined_ids, scope_instrs,
                        );
                        let _ = writeln!(out, "{pad}}}");
                    } else {
                        let mut vis2 = visited.clone();
                        self.emit_cfg_region(
                            body_bid, Some(fall_bid), indent, out,
                            &mut vis2, emitted_scopes, scope_index, instr_scope, inlined_ids, scope_instrs,
                        );
                    }
                    if Some(fall_bid) == stop_at {
                        return;
                    }
                    current = fall_bid;
                    continue;
                }

                Terminal::MaybeThrow { continuation, .. } => {
                    current = *continuation;
                    continue;
                }

                Terminal::Logical { test, fallthrough, .. }
                | Terminal::Ternary { test, fallthrough, .. }
                | Terminal::Optional { test, fallthrough, .. }
                | Terminal::Sequence { block: test, fallthrough, .. } => {
                    let test_bid = *test;
                    let fall_bid = *fallthrough;
                    let mut vis2 = visited.clone();
                    self.emit_cfg_region(
                        test_bid, Some(fall_bid), indent, out,
                        &mut vis2, emitted_scopes, scope_index, instr_scope, inlined_ids, scope_instrs,
                    );
                    if Some(fall_bid) == stop_at {
                        return;
                    }
                    current = fall_bid;
                    continue;
                }

                Terminal::ReactiveScope { block, fallthrough, .. }
                | Terminal::PrunedScope { block, fallthrough, .. } => {
                    let body_bid = *block;
                    let fall_bid = *fallthrough;
                    let mut vis2 = visited.clone();
                    self.emit_cfg_region(
                        body_bid, Some(fall_bid), indent, out,
                        &mut vis2, emitted_scopes, scope_index, instr_scope, inlined_ids, scope_instrs,
                    );
                    if Some(fall_bid) == stop_at {
                        return;
                    }
                    current = fall_bid;
                    continue;
                }

                _ => {
                    // For any other terminal, try to follow fallthrough.
                    if let Some(ft) = block.terminal.fallthrough() {
                        if Some(ft) == stop_at {
                            return;
                        }
                        current = ft;
                        continue;
                    }
                    return;
                }
            }
        }
    }

    /// Emit a reactive scope block (memoization if/else) and update inlined_exprs.
    #[allow(clippy::too_many_arguments)]
    fn emit_scope_block_inner(
        &mut self,
        scope_id: &ScopeId,
        instrs: &[&Instruction],
        indent: usize,
        scope_index: &mut usize,
        out: &mut String,
        inlined_ids: &std::collections::HashSet<u32>,
    ) {
        let pad = "  ".repeat(indent);
        let body_pad = "  ".repeat(indent + 1);

        let dep_slot_list = self.dep_slots.get(scope_id).cloned().unwrap_or_default();
        let out_slot_list = self.output_slots.get(scope_id).cloned().unwrap_or_else(|| vec![0]);
        let sentinel_slot = out_slot_list[0];

        // -----------------------------------------------------------------------
        // Promote used temporaries: detect scope member instructions that are
        // calls to primitive-returning globals (String, Number, etc.) whose only
        // reactive dep matches a scope dep, and hoist them as `const tN = ...`
        // before the scope block. The scope dep condition then uses `tN` instead
        // of the raw dep variable.
        //
        // Example: scope dep = `state`, scope member = `String(state)` (inlined
        // into JSX body). We emit `const t3 = String(state)` before the block
        // and use `$[3] !== t3` as the condition instead of `$[3] !== state`.
        //
        // IMPORTANT: This must run BEFORE `analyze_scope` so that the updated
        // `inlined_exprs` (replacing the call with `tN`) are visible when
        // `analyze_scope` computes `cache_expr` for the scope's outputs.
        // -----------------------------------------------------------------------
        let scope_deps: Vec<_> = self.env
            .scopes
            .get(scope_id)
            .map(|s| s.dependencies.clone())
            .unwrap_or_default();

        let has_deps = !scope_deps.is_empty();

        // dep_expr_overrides[i] = Some("tN") if dep i is promoted to a hoisted const.
        let mut dep_expr_overrides: Vec<Option<String>> = vec![None; scope_deps.len()];
        // pre_scope_lines: const declarations emitted before the scope block.
        let mut pre_scope_lines: Vec<String> = Vec::new();

        if has_deps {
            for (dep_idx, dep) in scope_deps.iter().enumerate() {
                let dep_base_id = dep.place.identifier;
                // For each dep, check if there's an inlined scope member that is a
                // CallExpression(primitive_global, [dep]) and whose result is used
                // inside the scope body.
                for instr in instrs.iter() {
                    let instr_id = instr.lvalue.identifier.0;
                    // Must be inlined (will be embedded in the body via inlined_exprs).
                    if !inlined_ids.contains(&instr_id) {
                        continue;
                    }
                    // Already promoted (dep_expr_overrides has a Some for another dep).
                    let already_promoted = dep_expr_overrides.iter().any(|o| {
                        o.as_deref().map(|t| self.inlined_exprs.get(&instr_id).map(|s| s == t).unwrap_or(false)).unwrap_or(false)
                    });
                    if already_promoted { continue; }
                    if let InstructionValue::CallExpression { callee, args, .. } = &instr.value {
                        // Check callee is a primitive-returning global.
                        let callee_global = self.instr_map.get(&callee.identifier.0)
                            .and_then(|ci| if let InstructionValue::LoadGlobal { binding, .. } = &ci.value {
                                Some(match binding {
                                    NonLocalBinding::Global { name } | NonLocalBinding::ModuleLocal { name }
                                    | NonLocalBinding::ImportDefault { name, .. }
                                    | NonLocalBinding::ImportNamespace { name, .. }
                                    | NonLocalBinding::ImportSpecifier { name, .. } => name.as_str(),
                                })
                            } else { None });
                        let Some(global_name) = callee_global else { continue; };
                        if !is_primitive_returning_global(global_name) { continue; }
                        // Check that args reduce to exactly the dep.
                        // Single-arg call where arg resolves to dep_base_id.
                        if args.len() != 1 { continue; }
                        let arg_id = match &args[0] {
                            CallArg::Place(p) => p.identifier,
                            CallArg::Spread(_) => continue,
                        };
                        // Resolve arg through LoadLocal/LoadContext chains.
                        let resolved_arg = resolve_through_loads(arg_id, &self.instr_map);
                        if resolved_arg != dep_base_id { continue; }
                        // Found a promotable instruction. Assign it a temp name.
                        let t_name = format!("t{}", *scope_index + self.param_name_offset);
                        *scope_index += 1;
                        // Emit `const tN = global(dep_expr);` before the scope block.
                        let dep_str = self.dep_expr(dep);
                        pre_scope_lines.push(format!("{pad}const {t_name} = {global_name}({dep_str});"));
                        // Override dep expression to use tN.
                        dep_expr_overrides[dep_idx] = Some(t_name.clone());
                        // Update inlined_exprs so that uses of this instr result → tN.
                        // This MUST happen before analyze_scope so cache_expr uses tN.
                        self.inlined_exprs.insert(instr_id, t_name);
                        break;
                    }
                }
            }
        }

        // Pre-compute dep expressions and allocate hoisted names BEFORE analyze_scope.
        // This ensures hoisted dep temps get lower indices (t0) and scope outputs get higher (t1).
        let mut hoisted_dep_info: Vec<(String, String)> = Vec::new();
        if has_deps {
            for (di, dep) in scope_deps.iter().enumerate() {
                let raw = dep_expr_overrides.get(di)
                    .and_then(|o| o.clone())
                    .unwrap_or_else(|| self.dep_expr(dep));
                let name = self.ident_name(scope_deps[di].place.identifier);
                let is_unnamed = name.starts_with("$t");
                // Hoist complex dep expressions to `const tN = expr;` before the scope block.
                // A dep is "complex" if it's unnamed AND not a simple identifier/property-chain.
                // This matches the reference compiler which hoists binary ops, calls, computed loads, etc.
                let is_complex = is_unnamed && {
                    // Simple: just an identifier or dotted chain like "props.foo.bar"
                    // Complex: contains operators, calls, brackets, template literals, etc.
                    let has_op = raw.contains(" + ") || raw.contains(" - ") || raw.contains(" * ")
                        || raw.contains(" / ") || raw.contains(" % ")
                        || raw.contains(" === ") || raw.contains(" !== ")
                        || raw.contains(" < ") || raw.contains(" > ")
                        || raw.contains(" <= ") || raw.contains(" >= ")
                        || raw.contains(" && ") || raw.contains(" || ")
                        || raw.contains(" ?? ");
                    let has_call_or_bracket = raw.contains('(') || raw.contains('[');
                    let has_template = raw.contains('`');
                    has_op || has_call_or_bracket || has_template
                };
                if is_complex {
                    let hoisted_name = format!("t{}", *scope_index + self.param_name_offset);
                    *scope_index += 1;
                    self.inlined_exprs.insert(scope_deps[di].place.identifier.0, hoisted_name.clone());
                    let old_expr = raw.clone();
                    let to_remove: Vec<u32> = self.inlined_exprs.iter()
                        .filter(|(_, v)| v.contains(&old_expr))
                        .map(|(&k, _)| k)
                        .collect();
                    for k in to_remove {
                        self.inlined_exprs.remove(&k);
                    }
                    hoisted_dep_info.push((raw, hoisted_name));
                } else {
                    hoisted_dep_info.push((raw.clone(), raw));
                }
            }
        }

        let mut analysis = self.analyze_scope(scope_id, instrs, inlined_ids, None);

        // After hoisting, update analysis.outputs.cache_expr to replace old expressions.
        for (orig, hoisted) in &hoisted_dep_info {
            if orig != hoisted {
                for output in &mut analysis.outputs {
                    output.cache_expr = output.cache_expr.replace(orig.as_str(), hoisted.as_str());
                }
            }
        }

        // Assign temp names to outputs.
        let mut output_cache_vars: Vec<String> = Vec::new();
        for output in &analysis.outputs {
            if output.is_named_var {
                output_cache_vars.push(
                    output.out_name.clone().unwrap_or_else(|| "undefined".to_string())
                );
            } else {
                let t = format!("t{}", *scope_index + self.param_name_offset);
                *scope_index += 1;
                // Map the value identifier of the skipped instruction to this temp name,
                // so references outside the scope resolve to tN instead of $tN.
                if let Some(skip_idx) = output.skip_idx {
                    if let Some(instr) = instrs.get(skip_idx) {
                        if let InstructionValue::StoreLocal { value, .. } = &instr.value {
                            self.scope_output_names.insert(value.identifier.0, t.clone());
                        }
                        self.scope_output_names.insert(instr.lvalue.identifier.0, t.clone());
                    }
                }
                output_cache_vars.push(t);
            }
        }
        if let Some(term_id) = analysis.terminal_place_id {
            let term_var = output_cache_vars.first().cloned().unwrap_or_else(|| {
                format!("t{}", *scope_index + self.param_name_offset - 1)
            });
            let term_expr = if let Some(ann) = &analysis.terminal_type_cast_annotation {
                format!("{term_var} as {ann}")
            } else {
                term_var
            };
            self.terminal_replacement.insert(term_id, term_expr);
        }

        let intra_set: std::collections::HashSet<usize> =
            analysis.intra_scope_stores.iter().copied().collect();
        let skip_set: std::collections::HashSet<usize> =
            analysis.outputs.iter().filter_map(|o| o.skip_idx).collect();

        let all_out_names: Vec<String> = analysis.outputs.iter()
            .filter(|o| o.is_named_var)
            .filter_map(|o| o.out_name.clone())
            .collect();

        // Emit any pre-scope const promotions.
        for line in &pre_scope_lines {
            let _ = writeln!(out, "{}", line);
        }

        // Build body lines (after hoisting so inlined_exprs is updated).
        let mut body_lines: Vec<String> = Vec::new();
        for (i, instr) in instrs.iter().enumerate() {
            if skip_set.contains(&i) { continue; }
            if inlined_ids.contains(&instr.lvalue.identifier.0) && !intra_set.contains(&i) {
                continue;
            }
            if intra_set.contains(&i) {
                if let InstructionValue::StoreLocal { lvalue, value, .. } = &instr.value {
                    let var_name = self.env
                        .get_identifier(lvalue.place.identifier)
                        .and_then(|id| id.name.as_ref())
                        .map(|_| self.ident_name(lvalue.place.identifier));
                    if let Some(n) = var_name {
                        let val_expr = self.expr(value);
                        // Skip self-assignments (e.g. `const _temp = _temp` from outlined fns).
                        if n == val_expr {
                            continue;
                        }
                        let stmt = if all_out_names.contains(&n) {
                            format!("{n} = {val_expr};")
                        } else {
                            match lvalue.kind {
                                InstructionKind::Reassign => format!("{n} = {val_expr};"),
                                InstructionKind::Const | InstructionKind::HoistedConst | InstructionKind::Function | InstructionKind::HoistedFunction => format!("const {n} = {val_expr};"),
                                _ => format!("let {n} = {val_expr};"),
                            }
                        };
                        body_lines.push(stmt);
                        continue;
                    }
                }
            }
            if let Some(s) = self.emit_stmt(instr, Some(*scope_id), &all_out_names) {
                body_lines.push(s);
            }
        }

        if has_deps {
            // Emit hoisted dep const declarations.
            for (orig, hoisted) in &hoisted_dep_info {
                if orig != hoisted {
                    let _ = writeln!(out, "{pad}const {hoisted} = {orig};");
                }
            }
            for cache_var in &output_cache_vars {
                let _ = writeln!(out, "{pad}let {cache_var};");
            }
            let cond_parts: Vec<String> = hoisted_dep_info.iter().zip(&dep_slot_list)
                .map(|((_, dep_str), &slot)| {
                    format!("$[{slot}] !== {dep_str}")
                })
                .collect();
            let condition = cond_parts.join(" || ");
            let _ = writeln!(out, "{pad}if ({condition}) {{");
            for line in &body_lines {
                let reindented = reindent_multiline(line, &body_pad);
                let _ = writeln!(out, "{body_pad}{reindented}");
            }
            for (output, cache_var) in analysis.outputs.iter().zip(&output_cache_vars) {
                if !output.is_named_var {
                    let expr_str = maybe_paren_jsx_scope_output(cache_var, &output.cache_expr);
                    let reindented = reindent_multiline(&expr_str, &body_pad);
                    let _ = writeln!(out, "{body_pad}{cache_var} = {};", reindented);
                }
            }
            for ((_, dep_str), &slot) in hoisted_dep_info.iter().zip(&dep_slot_list) {
                let _ = writeln!(out, "{body_pad}$[{slot}] = {dep_str};");
            }
            for (cache_var, &slot) in output_cache_vars.iter().zip(&out_slot_list) {
                let _ = writeln!(out, "{body_pad}$[{slot}] = {cache_var};");
            }
            let _ = writeln!(out, "{pad}}} else {{");
            for (cache_var, &slot) in output_cache_vars.iter().zip(&out_slot_list) {
                let _ = writeln!(out, "{body_pad}{cache_var} = $[{slot}];");
            }
            let _ = writeln!(out, "{pad}}}");
            for (output, cache_var) in analysis.outputs.iter().zip(&output_cache_vars) {
                if !output.is_named_var {
                    if let Some(ref name) = output.out_name {
                        let _ = writeln!(out, "{pad}{} {name} = {cache_var};", output.out_kw);
                    }
                }
            }
        } else {
            for cache_var in &output_cache_vars {
                let _ = writeln!(out, "{pad}let {cache_var};");
            }
            let _ = writeln!(out, "{pad}if ($[{sentinel_slot}] === Symbol.for(\"react.memo_cache_sentinel\")) {{");
            for line in &body_lines {
                let reindented = reindent_multiline(line, &body_pad);
                let _ = writeln!(out, "{body_pad}{reindented}");
            }
            for (output, cache_var) in analysis.outputs.iter().zip(&output_cache_vars) {
                if !output.is_named_var {
                    let expr_str = maybe_paren_jsx_scope_output(cache_var, &output.cache_expr);
                    let reindented = reindent_multiline(&expr_str, &body_pad);
                    let _ = writeln!(out, "{body_pad}{cache_var} = {};", reindented);
                }
            }
            for (cache_var, &slot) in output_cache_vars.iter().zip(&out_slot_list) {
                let _ = writeln!(out, "{body_pad}$[{slot}] = {cache_var};");
            }
            let _ = writeln!(out, "{pad}}} else {{");
            for (cache_var, &slot) in output_cache_vars.iter().zip(&out_slot_list) {
                let _ = writeln!(out, "{body_pad}{cache_var} = $[{slot}];");
            }
            let _ = writeln!(out, "{pad}}}");
            for (output, cache_var) in analysis.outputs.iter().zip(&output_cache_vars) {
                if !output.is_named_var {
                    if let Some(ref name) = output.out_name {
                        if name != cache_var {
                            let _ = writeln!(out, "{pad}{} {name} = {cache_var};", output.out_kw);
                        }
                    }
                }
            }
        }

        // After scope emission, override inlined_exprs for each skipped instruction.
        // Collect the old->new mappings so we can propagate them.
        let mut old_to_new: Vec<(String, String)> = Vec::new();
        for (output, cache_var) in analysis.outputs.iter().zip(&output_cache_vars) {
            if let Some(skip_i) = output.skip_idx {
                if let Some(skip_instr) = instrs.get(skip_i) {
                    let old_name = format!("$t{}", skip_instr.lvalue.identifier.0);
                    old_to_new.push((old_name, cache_var.clone()));
                    self.inlined_exprs.insert(skip_instr.lvalue.identifier.0, cache_var.clone());
                }
            }
        }
        // Propagate: update any inlined_exprs entries that still reference old $tN names.
        if !old_to_new.is_empty() {
            for value in self.inlined_exprs.values_mut() {
                for (old_name, new_name) in &old_to_new {
                    if value == old_name {
                        *value = new_name.clone();
                    }
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Loop-wrapped scope helpers
    // -----------------------------------------------------------------------

    /// Check if the loop body block (or its successors up to stop_at) contains
    /// instructions belonging to an unvisited scope. Returns the ScopeId if found.
    fn find_scope_for_loop_body(
        &self,
        loop_bid: BlockId,
        instr_scope: &HashMap<InstructionId, ScopeId>,
        emitted_scopes: &std::collections::HashSet<ScopeId>,
    ) -> Option<ScopeId> {
        // Walk blocks reachable from loop_bid (but not going back to loop_bid itself).
        // We only check the first loop body block for scope instructions.
        if let Some(block) = self.hir.body.blocks.get(&loop_bid) {
            for instr in &block.instructions {
                if let Some(&sid) = instr_scope.get(&instr.id) {
                    if !emitted_scopes.contains(&sid) {
                        return Some(sid);
                    }
                }
            }
        }
        None
    }

    /// Compute the set of all blocks reachable from `loop_bid` without passing
    /// through `stop_bids`. This gives the complete loop-body block set.
    fn compute_loop_body_blocks(
        &self,
        loop_bid: BlockId,
        stop_bids: &std::collections::HashSet<BlockId>,
    ) -> std::collections::HashSet<BlockId> {
        let mut reachable = std::collections::HashSet::new();
        let mut queue = vec![loop_bid];
        while let Some(bid) = queue.pop() {
            if stop_bids.contains(&bid) { continue; }
            if !reachable.insert(bid) { continue; }
            if let Some(block) = self.hir.body.blocks.get(&bid) {
                for succ in block.terminal.successors() {
                    if !reachable.contains(&succ) {
                        queue.push(succ);
                    }
                }
            }
        }
        reachable
    }

    /// Collect the "pre-loop" instructions from a scope: instructions that belong
    /// to the scope but are NOT in any loop body block (reachable from loop_bid
    /// without going through test_bid or fallthrough).
    fn collect_pre_loop_scope_instrs(
        &self,
        scope_instrs_list: &[Instruction],
        loop_bid: BlockId,
        test_bid: Option<BlockId>,
    ) -> Vec<Instruction> {
        // Build the full set of loop-body blocks (all blocks reachable from loop_bid
        // without crossing through test_bid).
        let mut stop_bids = std::collections::HashSet::new();
        if let Some(t) = test_bid { stop_bids.insert(t); }
        let loop_body_blocks = self.compute_loop_body_blocks(loop_bid, &stop_bids);

        scope_instrs_list.iter().filter(|instr| {
            let blk = self.instr_to_block.get(&instr.id).copied();
            let in_loop_body = blk.map(|b| loop_body_blocks.contains(&b)).unwrap_or(false);
            let in_test = test_bid.map(|t| blk == Some(t)).unwrap_or(false);
            !in_loop_body && !in_test
        }).cloned().collect()
    }

    /// Emit a loop terminal (DoWhile/While/For) wrapped in a scope block.
    /// This is called when the loop body contains instructions belonging to
    /// an unvisited reactive scope. The scope's memoization if/else wraps
    /// the entire loop structure.
    ///
    /// Returns the fallthrough BlockId to continue at after the scope+loop.
    #[allow(clippy::too_many_arguments)]
    fn emit_loop_wrapped_scope(
        &mut self,
        sid: ScopeId,
        pre_loop_instrs: &[Instruction],
        loop_bid: BlockId,
        test_bid: BlockId,
        fall_bid: BlockId,
        loop_type: LoopType,
        indent: usize,
        scope_index: &mut usize,
        out: &mut String,
        visited: &mut std::collections::HashSet<BlockId>,
        emitted_scopes: &mut std::collections::HashSet<ScopeId>,
        instr_scope: &HashMap<InstructionId, ScopeId>,
        inlined_ids: &mut std::collections::HashSet<u32>,
        scope_instrs: &HashMap<ScopeId, Vec<Instruction>>,
    ) {
        let pad = "  ".repeat(indent);
        let body_pad = "  ".repeat(indent + 1);

        let dep_slot_list = self.dep_slots.get(&sid).cloned().unwrap_or_default();
        let out_slot_list = self.output_slots.get(&sid).cloned().unwrap_or_else(|| vec![0]);
        let sentinel_slot = out_slot_list[0];

        // Get all scope instructions for analysis.
        let scope_instrs_list = scope_instrs.get(&sid).cloned().unwrap_or_default();
        let scope_instr_refs: Vec<&Instruction> = scope_instrs_list.iter().collect();
        let analysis = self.analyze_scope(&sid, &scope_instr_refs, inlined_ids, Some(fall_bid));

        // Assign temp names to outputs.
        let mut output_cache_vars: Vec<String> = Vec::new();
        for output in &analysis.outputs {
            if output.is_named_var {
                output_cache_vars.push(
                    output.out_name.clone().unwrap_or_else(|| "undefined".to_string())
                );
            } else {
                let t = format!("t{}", *scope_index + self.param_name_offset);
                *scope_index += 1;
                if let Some(skip_idx) = output.skip_idx {
                    if let Some(instr) = scope_instr_refs.get(skip_idx) {
                        if let InstructionValue::StoreLocal { value, .. } = &instr.value {
                            self.scope_output_names.insert(value.identifier.0, t.clone());
                        }
                        self.scope_output_names.insert(instr.lvalue.identifier.0, t.clone());
                    }
                }
                output_cache_vars.push(t);
            }
        }
        if let Some(term_id) = analysis.terminal_place_id {
            let term_var = output_cache_vars.first().cloned().unwrap_or_else(|| {
                format!("t{}", *scope_index + self.param_name_offset - 1)
            });
            let term_expr = if let Some(ann) = &analysis.terminal_type_cast_annotation {
                format!("{term_var} as {ann}")
            } else {
                term_var
            };
            self.terminal_replacement.insert(term_id, term_expr);
        }

        let intra_set: std::collections::HashSet<usize> =
            analysis.intra_scope_stores.iter().copied().collect();
        let skip_set: std::collections::HashSet<usize> =
            analysis.outputs.iter().filter_map(|o| o.skip_idx).collect();

        let scope_deps: Vec<_> = self.env
            .scopes
            .get(&sid)
            .map(|s| s.dependencies.clone())
            .unwrap_or_default();
        let has_deps = !scope_deps.is_empty();

        let all_out_names: Vec<String> = analysis.outputs.iter()
            .filter(|o| o.is_named_var)
            .filter_map(|o| o.out_name.clone())
            .collect();

        // Emit let declarations for outputs.
        for cache_var in &output_cache_vars {
            let _ = writeln!(out, "{pad}let {cache_var};");
        }

        // Emit the scope condition (dep check or sentinel check).
        if has_deps {
            let cond_parts: Vec<String> = scope_deps.iter().zip(&dep_slot_list)
                .map(|(dep, &slot)| format!("$[{slot}] !== {}", self.dep_expr(dep)))
                .collect();
            let condition = cond_parts.join(" || ");
            let _ = writeln!(out, "{pad}if ({condition}) {{");
        } else {
            let _ = writeln!(out, "{pad}if ($[{sentinel_slot}] === Symbol.for(\"react.memo_cache_sentinel\")) {{");
        }

        // Emit pre-loop scope instructions inside the "new" branch.
        // These are instructions in the scope that are NOT in the loop body blocks.
        // We need to look up each pre-loop instr's original index in scope_instr_refs
        // so that skip_set and intra_set (which use original indices) work correctly.
        if std::env::var("RC_DEBUG").is_ok() {
            eprintln!("[loop_wrap] scope={:?} pre_loop_instrs.len()={} scope_instr_refs.len()={}", sid.0, pre_loop_instrs.len(), scope_instr_refs.len());
            for instr in pre_loop_instrs.iter() {
                eprintln!("  pre_loop instr[{}] lv=${} in_block={:?}", instr.id.0, instr.lvalue.identifier.0, self.instr_to_block.get(&instr.id));
            }
            eprintln!("  loop_bid={:?} test_bid={:?} entry={:?}", loop_bid, test_bid, self.hir.body.entry);
            eprintln!("  all scope instrs:");
            for instr in &scope_instr_refs {
                eprintln!("    instr[{}] lv=${} in_block={:?}", instr.id.0, instr.lvalue.identifier.0, self.instr_to_block.get(&instr.id));
            }
            eprintln!("  hir blocks:");
            for (bid, block) in &self.hir.body.blocks {
                eprintln!("    block {:?} term={:?} instrs.len()={}", bid, std::mem::discriminant(&block.terminal), block.instructions.len());
            }
        }
        for instr in pre_loop_instrs.iter() {
            // Find original index in scope_instr_refs.
            let orig_idx = scope_instr_refs.iter().position(|r| r.id == instr.id);
            let i = orig_idx.unwrap_or(usize::MAX);
            if skip_set.contains(&i) { continue; }
            if inlined_ids.contains(&instr.lvalue.identifier.0) && !intra_set.contains(&i) {
                continue;
            }
            if intra_set.contains(&i) {
                if let InstructionValue::StoreLocal { lvalue, value, .. } = &instr.value {
                    // Use ident_name so name_overrides (shadowing renaming) is applied.
                    let var_name = self.env
                        .get_identifier(lvalue.place.identifier)
                        .and_then(|id| id.name.as_ref())
                        .map(|_| self.ident_name(lvalue.place.identifier));
                    if let Some(n) = var_name {
                        let val_expr = self.expr(value);
                        // Skip self-assignments (e.g. `const _temp = _temp` from outlined fns).
                        if n == val_expr {
                            continue;
                        }
                        // If this variable is a hoisted named output (let ret; outside),
                        // emit a plain assignment rather than a declaration.
                        let stmt = if all_out_names.contains(&n) {
                            format!("{n} = {val_expr};")
                        } else {
                            match lvalue.kind {
                                InstructionKind::Reassign => format!("{n} = {val_expr};"),
                                InstructionKind::Const | InstructionKind::HoistedConst | InstructionKind::Function | InstructionKind::HoistedFunction => format!("const {n} = {val_expr};"),
                                _ => format!("let {n} = {val_expr};"),
                            }
                        };
                        let reindented = reindent_multiline(&stmt, &body_pad);
                        let _ = writeln!(out, "{body_pad}{reindented}");
                        continue;
                    }
                }
            }
            if let Some(s) = self.emit_stmt(instr, Some(sid), &all_out_names) {
                let reindented = reindent_multiline(&s, &body_pad);
                let _ = writeln!(out, "{body_pad}{reindented}");
            }
        }

        // Mark scope as emitted and add to within_loop_scopes so body
        // instructions are emitted as plain statements.
        emitted_scopes.insert(sid);
        self.within_loop_scopes.insert(sid);

        // Emit the loop itself inside the "new" branch.
        let loop_body_indent = indent + 1;
        let loop_body_pad = "  ".repeat(loop_body_indent);
        let loop_inner_indent = indent + 2;

        match loop_type {
            LoopType::DoWhile => {
                // Get test expression.
                visited.insert(test_bid);
                let test_expr = self.hir.body.blocks.get(&test_bid).and_then(|b| {
                    if let Terminal::Branch { test, .. } = &b.terminal {
                        Some(self.expr(test))
                    } else { None }
                }).unwrap_or_else(|| "true".to_string());

                let _ = writeln!(out, "{loop_body_pad}do {{");
                let mut vis2 = visited.clone();
                vis2.insert(test_bid);
                self.emit_cfg_region(
                    loop_bid, Some(test_bid), loop_inner_indent, out,
                    &mut vis2, emitted_scopes, scope_index, instr_scope, inlined_ids, scope_instrs,
                );
                let _ = writeln!(out, "{loop_body_pad}}} while ({test_expr});");
            }
            LoopType::While { test_bid: while_test_bid } => {
                // test_bid == while_test_bid for While loops
                visited.insert(while_test_bid);
                let test_expr = self.hir.body.blocks.get(&while_test_bid).and_then(|b| {
                    if let Terminal::Branch { test, .. } = &b.terminal {
                        Some(self.expr(test))
                    } else { None }
                }).unwrap_or_else(|| "true".to_string());
                // Emit test block instructions (condition computations).
                let test_block_instrs = self.hir.body.blocks.get(&while_test_bid)
                    .map(|b| b.instructions.clone())
                    .unwrap_or_default();
                for instr in &test_block_instrs {
                    if inlined_ids.contains(&instr.lvalue.identifier.0) { continue; }
                    if self.inlined_exprs.contains_key(&instr.lvalue.identifier.0) { continue; }
                    if let Some(s) = self.emit_stmt(instr, None, &[]) {
                        for line in s.lines() {
                            let _ = writeln!(out, "{loop_body_pad}{line}");
                        }
                    }
                }
                let _ = writeln!(out, "{loop_body_pad}while ({test_expr}) {{");
                let mut vis2 = visited.clone();
                vis2.insert(while_test_bid);
                self.emit_cfg_region(
                    loop_bid, Some(while_test_bid), loop_inner_indent, out,
                    &mut vis2, emitted_scopes, scope_index, instr_scope, inlined_ids, scope_instrs,
                );
                let _ = writeln!(out, "{loop_body_pad}}}");
            }
            LoopType::ForOf { test_bid: fo_test_bid, iterable_expr, loop_var_name, binding_id } => {
                // Mark test block as visited so the recursive body walk doesn't re-enter it.
                visited.insert(fo_test_bid);
                // Map IteratorNext result → loop var name so inner loads resolve correctly.
                let iter_next_id = self.hir.body.blocks.get(&fo_test_bid).and_then(|b| {
                    b.instructions.iter().rev().find_map(|instr| {
                        if matches!(&instr.value, InstructionValue::IteratorNext { .. }) {
                            Some(instr.lvalue.identifier.0)
                        } else {
                            None
                        }
                    })
                });
                if let Some(iter_id) = iter_next_id {
                    self.inlined_exprs.insert(iter_id, loop_var_name.clone());
                }
                if binding_id != 0 {
                    inlined_ids.insert(binding_id);
                }
                let _ = writeln!(out, "{loop_body_pad}for (const {loop_var_name} of {iterable_expr}) {{");
                let mut vis2 = visited.clone();
                vis2.insert(fo_test_bid);
                self.emit_cfg_region(
                    loop_bid, Some(fo_test_bid), loop_inner_indent, out,
                    &mut vis2, emitted_scopes, scope_index, instr_scope, inlined_ids, scope_instrs,
                );
                let _ = writeln!(out, "{loop_body_pad}}}");
            }
            LoopType::ForIn { iterable_expr, loop_var_name, binding_id } => {
                if binding_id != 0 {
                    inlined_ids.insert(binding_id);
                }
                let _ = writeln!(out, "{loop_body_pad}for (const {loop_var_name} in {iterable_expr}) {{");
                let mut vis2 = visited.clone();
                self.emit_cfg_region(
                    loop_bid, None, loop_inner_indent, out,
                    &mut vis2, emitted_scopes, scope_index, instr_scope, inlined_ids, scope_instrs,
                );
                let _ = writeln!(out, "{loop_body_pad}}}");
            }
        }

        // Remove from within_loop_scopes now that the loop body is done.
        self.within_loop_scopes.remove(&sid);

        // Emit scope cache stores.
        for (output, cache_var) in analysis.outputs.iter().zip(&output_cache_vars) {
            if !output.is_named_var {
                let expr_str = maybe_paren_jsx_scope_output(cache_var, &output.cache_expr);
                let reindented = reindent_multiline(&expr_str, &body_pad);
                let _ = writeln!(out, "{body_pad}{cache_var} = {};", reindented);
            }
        }
        if has_deps {
            for (dep, &slot) in scope_deps.iter().zip(&dep_slot_list) {
                let _ = writeln!(out, "{body_pad}$[{slot}] = {};", self.dep_expr(dep));
            }
        }
        for (cache_var, &slot) in output_cache_vars.iter().zip(&out_slot_list) {
            let _ = writeln!(out, "{body_pad}$[{slot}] = {cache_var};");
        }

        // Emit else branch.
        let _ = writeln!(out, "{pad}}} else {{");
        for (cache_var, &slot) in output_cache_vars.iter().zip(&out_slot_list) {
            let _ = writeln!(out, "{body_pad}{cache_var} = $[{slot}];");
        }
        let _ = writeln!(out, "{pad}}}");

        // Emit post-scope declarations if needed.
        for (output, cache_var) in analysis.outputs.iter().zip(&output_cache_vars) {
            if !output.is_named_var {
                if let Some(ref name) = output.out_name {
                    let _ = writeln!(out, "{pad}{} {name} = {cache_var};", output.out_kw);
                }
            }
        }

        // Override inlined_exprs for skipped instructions.
        let mut old_to_new: Vec<(String, String)> = Vec::new();
        for (output, cache_var) in analysis.outputs.iter().zip(&output_cache_vars) {
            if let Some(skip_i) = output.skip_idx {
                if let Some(skip_instr) = scope_instr_refs.get(skip_i) {
                    let old_name = format!("$t{}", skip_instr.lvalue.identifier.0);
                    old_to_new.push((old_name, cache_var.clone()));
                    self.inlined_exprs.insert(skip_instr.lvalue.identifier.0, cache_var.clone());
                }
            }
        }
        if !old_to_new.is_empty() {
            for value in self.inlined_exprs.values_mut() {
                for (old_name, new_name) in &old_to_new {
                    if value == old_name {
                        *value = new_name.clone();
                    }
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Inline map construction
    // -----------------------------------------------------------------------

    fn build_inline_map(&mut self, instrs: &[Instruction]) {
        // Pass 1: mark transparent loads.
        for instr in instrs {
            match &instr.value {
                InstructionValue::LoadGlobal { binding, .. } => {
                    let s = self.binding_name(binding);
                    self.inlined_exprs.insert(instr.lvalue.identifier.0, s);
                }
                InstructionValue::LoadLocal { place, .. }
                | InstructionValue::LoadContext { place, .. } => {
                    let s = self.ident_name(place.identifier);
                    self.inlined_exprs.insert(instr.lvalue.identifier.0, s);
                }
                _ => {}
            }
        }

        // Pass 1.5: build adjusted use counts for MethodCall receiver inlining.
        // When we have `receiver.method(args)`, the HIR has:
        //   t_method = PropertyLoad(receiver, 'method')
        //   MethodCall(receiver, t_method, args)
        // The receiver gets use_count=2 (PropertyLoad + MethodCall), preventing inlining.
        // But these two uses are one conceptual expression: `receiver.method(...)`.
        // For each PropertyLoad whose result is used only as MethodCall.property,
        // subtract 1 from the receiver's effective use count.
        let mut adjusted_use_count = self.use_count.clone();
        // Collect PropertyLoad IDs used as MethodCall.property.
        let mut method_prop_ids: std::collections::HashSet<u32> = std::collections::HashSet::new();
        for instr in instrs {
            if let InstructionValue::MethodCall { property, .. } = &instr.value {
                method_prop_ids.insert(property.identifier.0);
            }
        }
        // For each such PropertyLoad, subtract 1 from its object's adjusted use count.
        for instr in instrs {
            if method_prop_ids.contains(&instr.lvalue.identifier.0) {
                if let InstructionValue::PropertyLoad { object, .. } = &instr.value {
                    if let Some(cnt) = adjusted_use_count.get_mut(&object.identifier.0) {
                        *cnt = cnt.saturating_sub(1);
                    }
                }
            }
        }

        // Pass 2: for single-use temps whose only use is as the value in
        // another instruction, inline the expression chain.
        // We do a topological walk (instructions are already in order).
        for instr in instrs {
            // Skip if already inlined.
            if self.inlined_exprs.contains_key(&instr.lvalue.identifier.0) {
                continue;
            }
            // Only inline unnamed temporaries that are used exactly once.
            let is_named = self.env
                .get_identifier(instr.lvalue.identifier)
                .and_then(|i| i.name.as_ref())
                .is_some();
            if is_named {
                continue;
            }
            let uses = *adjusted_use_count.get(&instr.lvalue.identifier.0).unwrap_or(&0);
            if uses != 1 {
                continue;
            }
            // Check if we can compute an inlinable expression.
            if let Some(expr) = self.try_inline_instr(instr) {
                self.inlined_exprs.insert(instr.lvalue.identifier.0, expr);
            }
        }

    }

    /// Returns true if any Place operand of `value` belongs to a reactive scope.
    fn any_operand_has_scope(&self, value: &InstructionValue) -> bool {
        use crate::hir::visitors::each_instruction_value_operand;
        for place in each_instruction_value_operand(value) {
            if self.env.get_identifier(place.identifier)
                .and_then(|i| i.scope)
                .is_some()
            {
                return true;
            }
        }
        false
    }

    fn try_inline_instr(&self, instr: &Instruction) -> Option<String> {
        match &instr.value {
            InstructionValue::PropertyLoad { object, property, .. } => {
                let obj = self.expr(object);
                Some(format!("{obj}.{property}"))
            }
            InstructionValue::CallExpression { callee, args, .. } => {
                let callee_expr = self.expr(callee);
                // If this is an immediately-invoked no-arg arrow, unwrap: `() => { return EXPR; }`
                // called with no args → emit just EXPR instead of (() => { return EXPR; })()
                if args.is_empty() {
                    if let Some(inner) = extract_iife_return_expr(&callee_expr) {
                        return Some(inner);
                    }
                }
                // Wrap arrow function callees in parens to produce ((x) => expr)() not (x) => expr()
                let callee_str = if callee_expr.contains("=>") {
                    format!("({callee_expr})")
                } else {
                    callee_expr
                };
                let args_expr = self.call_args(args);
                Some(format!("{callee_str}({args_expr})"))
            }
            InstructionValue::MethodCall { receiver, property, args, .. } => {
                let recv = self.expr(receiver);
                let method_suffix = self.method_suffix_from_place(property);
                let args_expr = self.call_args(args);
                Some(format!("{recv}{method_suffix}({args_expr})"))
            }
            InstructionValue::ArrayExpression { elements, .. } => {
                let elems = elements.iter().map(|e| match e {
                    ArrayElement::Place(p) => self.expr(p),
                    ArrayElement::Spread(s) => format!("...{}", self.expr(&s.place)),
                    ArrayElement::Hole => String::new(),
                }).collect::<Vec<_>>().join(", ");
                Some(format!("[{elems}]"))
            }
            InstructionValue::ObjectExpression { properties, .. } => {
                let props = properties.iter().map(|p| match p {
                    ObjectExpressionProperty::Property(op) => {
                        self.emit_object_property(op)
                    }
                    ObjectExpressionProperty::Spread(s) => {
                        format!("...{}", self.expr(&s.place))
                    }
                }).collect::<Vec<_>>().join(", ");
                if props.is_empty() {
                    Some("{}".to_string())
                } else {
                    Some(format!("{{ {props} }}"))
                }
            }
            InstructionValue::BinaryExpression { operator, left, right, .. } => {
                let op = binary_op_str(operator);
                let l = self.expr(left);
                let r = self.expr(right);
                Some(format!("{l} {op} {r}"))
            }
            InstructionValue::TernaryExpression { test, consequent, alternate, .. } => {
                let t = self.expr(test);
                let c = self.expr(consequent);
                let a = self.expr(alternate);
                Some(format!("{t} ? {c} : {a}"))
            }
            InstructionValue::UnaryExpression { operator, value, .. } => {
                let v = self.expr(value);
                let op = unary_op_prefix(operator);
                Some(format!("{op}{v}"))
            }
            InstructionValue::Primitive { value, .. } => {
                Some(primitive_expr(value))
            }
            InstructionValue::TypeCastExpression { value, source_annotation, .. } => {
                let v = self.expr(value);
                if let Some(ann) = source_annotation {
                    Some(format!("{v} as {ann}"))
                } else {
                    Some(v)
                }
            }
            InstructionValue::ComputedLoad { object, property, .. } => {
                let obj = self.expr(object);
                let prop = self.expr(property);
                // If the property is a string literal, emit `obj.key` (dot notation).
                // Otherwise emit `obj[expr]`.
                if let Some(key) = extract_string_literal(&prop) {
                    if is_valid_identifier(&key) {
                        return Some(format!("{obj}.{key}"));
                    }
                }
                Some(format!("{obj}[{prop}]"))
            }
            InstructionValue::NewExpression { callee, args, .. } => {
                let callee_expr = self.expr(callee);
                let args_expr = self.call_args(args);
                Some(format!("new {callee_expr}({args_expr})"))
            }
            InstructionValue::TemplateLiteral { quasis, subexprs, .. } => {
                // Empty template literal → ""
                if subexprs.is_empty() && quasis.iter().all(|q| q.raw.is_empty()) {
                    return Some("\"\"".to_string());
                }
                let mut parts = Vec::new();
                let mut qi = quasis.iter();
                if let Some(q) = qi.next() {
                    parts.push(q.raw.clone());
                }
                for sub in subexprs {
                    let sub_expr = self.expr(sub);
                    parts.push(format!("${{{sub_expr}}}"));
                    if let Some(q) = qi.next() {
                        parts.push(q.raw.clone());
                    }
                }
                Some(format!("`{}`", parts.join("")))
            }
            InstructionValue::JsxExpression { tag, props, children, .. } => {
                let tag_str = match tag {
                    JsxTag::Builtin(b) => b.name.clone(),
                    JsxTag::Place(p) => self.expr(p),
                };
                let attr_parts: Vec<String> = props.iter().map(|attr| match attr {
                    JsxAttribute::Attribute { name, place } => {
                        let val = self.expr(place);
                        // If the value is a double-quoted JS string literal that contains an
                        // escaped double-quote (`\"`), emit as a single-quoted JSX expression
                        // to match React compiler / Babel output.
                        // e.g. "Some \"text\"" → {'Some "text"'}
                        if val.starts_with('"') && val.ends_with('"') && val.len() >= 2 {
                            let inner = &val[1..val.len()-1];
                            // `inner` is the body of a double-quoted JS string:
                            //   - `\"` means the original string had a double-quote character.
                            //   - `\\` means the original string had a backslash.
                            // We trigger the single-quote conversion if `inner` contains `\"`.
                            if inner.contains("\\\"") {
                                // Convert from double-quoted body to single-quoted body:
                                // `\"` → `"` (unescaped in single-quote context)
                                // `'` → `\'` (must escape single-quotes)
                                // All other escape sequences kept as-is.
                                let mut result = String::with_capacity(inner.len());
                                let bytes = inner.as_bytes();
                                let mut i = 0;
                                while i < bytes.len() {
                                    if bytes[i] == b'\\' && i + 1 < bytes.len() {
                                        if bytes[i + 1] == b'"' {
                                            // `\"` → `"` in single-quoted string
                                            result.push('"');
                                            i += 2;
                                        } else {
                                            // Other escape sequence: keep as-is
                                            result.push('\\');
                                            result.push(bytes[i + 1] as char);
                                            i += 2;
                                        }
                                    } else if bytes[i] == b'\'' {
                                        result.push_str("\\'");
                                        i += 1;
                                    } else {
                                        result.push(bytes[i] as char);
                                        i += 1;
                                    }
                                }
                                format!("{name}={{'{result}'}}")
                            } else {
                                format!("{name}={val}")
                            }
                        } else {
                            format!("{name}={{{val}}}")
                        }
                    }
                    JsxAttribute::Spread { argument } => format!("{{...{}}}", self.expr(argument)),
                }).collect();
                match children {
                    Some(ch) if !ch.is_empty() => {
                        let c = ch.iter().map(|c| self.jsx_child_expr(c)).collect::<Vec<_>>().join("");
                        if attr_parts.is_empty() {
                            Some(format!("<{tag_str}>{c}</{tag_str}>"))
                        } else {
                            Some(format!("<{tag_str} {}>{c}</{tag_str}>", attr_parts.join(" ")))
                        }
                    }
                    _ => {
                        if attr_parts.is_empty() {
                            Some(format!("<{tag_str} />"))
                        } else {
                            Some(format!("<{tag_str} {} />", attr_parts.join(" ")))
                        }
                    }
                }
            }
            InstructionValue::JsxFragment { children, .. } => {
                let ch = children.iter().map(|c| self.jsx_child_expr(c)).collect::<Vec<_>>().join("");
                Some(format!("<>{ch}</>"))
            }
            InstructionValue::Await { value, .. } => {
                let v = self.expr(value);
                Some(format!("await {v}"))
            }
            InstructionValue::FunctionExpression { name, name_hint, fn_type, lowered_func, .. } => {
                // If this function was outlined, emit just the reference name.
                if let Some(hint) = name_hint {
                    return Some(hint.clone());
                }
                // Only inline if we have the original source text (avoids empty-body stubs).
                let src = &lowered_func.func.original_source;
                if !src.is_empty() {
                    // Apply capture renames: if any outer variable captured by this closure
                    // was renamed (due to shadowing), update the body text to use the new name.
                    let src = apply_capture_renames(src, &lowered_func.func.context, self.env, &self.name_overrides);
                    // Normalize: arrow functions with a single unparenthesized param get parens added.
                    // e.g. `e => ...` → `(e) => ...`  (TS compiler always parenthesizes)
                    // Also normalize body text: single quotes → double, computed property → dot.
                    if matches!(fn_type, FunctionExpressionType::Arrow) {
                        let normalized = normalize_arrow_params(&src);
                        let normalized = normalize_fn_body_text(&normalized);
                        return Some(normalized);
                    }
                    return Some(normalize_fn_body_text(&src));
                }
                // Fallback: emit a stub. This should rarely happen.
                let async_kw = if lowered_func.func.async_ { "async " } else { "" };
                let fn_name_str = name.as_deref().unwrap_or("");
                match fn_type {
                    FunctionExpressionType::Arrow => Some(format!("{async_kw}() => {{}}")),
                    _ => Some(format!("{async_kw}function {fn_name_str}() {{}}")),
                }
            }
            InstructionValue::InlineJs { source, .. } => {
                Some(source.clone())
            }
            InstructionValue::TaggedTemplateExpression { tag, quasi, .. } => {
                let tag_expr = self.expr(tag);
                Some(format!("{tag_expr}`{}`", quasi.raw))
            }
            _ => None,
        }
    }

    /// Returns the set of identifier IDs that are fully inlined and should
    /// NOT produce standalone statements.
    fn collect_inlined_ids(&self, instrs: &[Instruction]) -> std::collections::HashSet<u32> {
        instrs.iter()
            .filter(|i| self.inlined_exprs.contains_key(&i.lvalue.identifier.0))
            .map(|i| i.lvalue.identifier.0)
            .collect()
    }

    // -----------------------------------------------------------------------
    // Scope output analysis
    // -----------------------------------------------------------------------


    // For loop-wrapped scopes, `fallthrough_bid` is the block after the loop.
    // StoreLocals in this block are post-loop assignments = scope outputs (must hoist as `let`).
    fn analyze_scope(
        &self,
        scope_id: &ScopeId,
        instrs: &[&Instruction],
        inlined_ids: &std::collections::HashSet<u32>,
        fallthrough_bid: Option<BlockId>,
    ) -> ScopeOutput {
        // Compute the set of lvalue identifier ids in this scope.
        let scope_lvalue_ids: std::collections::HashSet<u32> =
            instrs.iter().map(|i| i.lvalue.identifier.0).collect();

        // For each StoreLocal in the scope, check if the named variable escapes.
        // store_local_info: (idx, name, value_expr, is_intra_scope)
        let mut store_local_info: Vec<(usize, Option<String>, String, bool)> = Vec::new();
        for (idx, instr) in instrs.iter().enumerate() {
            if let InstructionValue::StoreLocal { lvalue, value, .. } = &instr.value {
                let var_id = lvalue.place.identifier;
                // Use ident_name so name_overrides (shadowing) is applied.
                let var_name = self.env
                    .get_identifier(var_id)
                    .and_then(|i| i.name.as_ref())
                    .map(|_| self.ident_name(var_id));
                // Prefer fresh try_inline_instr over potentially-stale inlined_exprs entry
                // from build_inline_map (which computed expressions before prior scopes emitted).
                let value_expr = instrs.iter()
                    .find(|i| i.lvalue.identifier == value.identifier)
                    .and_then(|vi| self.try_inline_instr(vi))
                    .unwrap_or_else(|| self.expr(value));
                let mut used_outside = self.is_var_used_outside_scope(var_id, &scope_lvalue_ids);
                // For loop-wrapped scopes: if the StoreLocal is in the fallthrough block
                // (the block executed after the loop terminates), it IS a scope output.
                // The fallthrough block stores the loop's final value into the named var
                // (e.g., `ret = phi_of_ret`) — this is the value that must be hoisted.
                if !used_outside {
                    if let Some(fall_bid) = fallthrough_bid {
                        let instr_block = self.instr_to_block.get(&instr.id).copied();
                        if instr_block == Some(fall_bid) {
                            used_outside = true;
                        }
                    }
                }
                // Also check phi chain for let-kind variables (handles vars where the kind
                // was preserved as Let/Reassign despite rewrite_instruction_kinds).
                if !used_outside && matches!(lvalue.kind,
                    InstructionKind::Let | InstructionKind::HoistedLet | InstructionKind::Reassign
                ) {
                    used_outside = self.is_let_var_phi_escaped(var_id, &scope_lvalue_ids);
                }
                if std::env::var("RC_DEBUG").is_ok() {
                    eprintln!("[analyze_scope] StoreLocal idx={} var_id={} name={:?} kind={:?} fallthrough_bid={:?} instr_block={:?} used_outside={}",
                        idx, var_id.0, var_name, lvalue.kind,
                        fallthrough_bid, self.instr_to_block.get(&instr.id), used_outside);
                }
                store_local_info.push((idx, var_name, value_expr, !used_outside));
            }
        }

        let intra_scope_stores: Vec<usize> = store_local_info.iter()
            .filter(|(_, _, _, intra)| *intra)
            .map(|(i, _, _, _)| *i)
            .collect();

        // Check for terminal feed (scope captures the function's return value).
        let terminal_feed = self.find_terminal_feed_instr(instrs);
        if let Some((feed_idx, feed_id, feed_expr)) = terminal_feed {
            // Terminal-feed case: scope output is the return value.
            // If there's also an escaping StoreLocal, treat it as an additional output.
            // For simplicity, use the single-output path (escaping StoreLocal takes priority).
            let last_escaping = store_local_info.iter().rev().find(|(_, _, _, intra)| !*intra);
            if let Some((esc_idx, esc_name, esc_value_expr, _)) = last_escaping {
                let (esc_var_id, esc_lvalue_kind) = instrs.get(*esc_idx).and_then(|i| {
                    if let InstructionValue::StoreLocal { lvalue, .. } = &i.value {
                        Some((lvalue.place.identifier, lvalue.kind))
                    } else { None }
                }).unzip();
                let is_let_kind = esc_lvalue_kind.map(|k| matches!(k,
                    InstructionKind::Let | InstructionKind::HoistedLet | InstructionKind::Reassign
                )).unwrap_or(false);
                let used_after = esc_var_id.map(|vid| {
                    instrs.iter().skip(*esc_idx + 1).any(|i| {
                        if let InstructionValue::LoadLocal { place, .. } = &i.value {
                            if place.identifier == vid {
                                let rid = i.lvalue.identifier.0;
                                if rid == feed_id { return false; }
                                return *self.use_count.get(&rid).unwrap_or(&0) > 0;
                            }
                        }
                        false
                    })
                }).unwrap_or(false);
                let captured_and_called = esc_var_id.map(|vid| {
                    let var_decl_id = self.env.get_identifier(vid)
                        .map(|i| i.declaration_id);
                    let has_call = instrs.iter().any(|i| matches!(&i.value,
                        InstructionValue::CallExpression { .. } | InstructionValue::MethodCall { .. }
                    ));
                    if !has_call { return false; }
                    instrs.iter().any(|i| {
                        if let InstructionValue::FunctionExpression { lowered_func, .. }
                            | InstructionValue::ObjectMethod { lowered_func, .. } = &i.value
                        {
                            lowered_func.func.context.iter().any(|ctx| {
                                if let Some(d) = var_decl_id {
                                    self.env.get_identifier(ctx.identifier)
                                        .map(|ci| ci.declaration_id == d)
                                        .unwrap_or(false)
                                } else {
                                    ctx.identifier == vid
                                }
                            })
                        } else {
                            false
                        }
                    })
                }).unwrap_or(false);
                let esc_intra: Vec<usize> = store_local_info.iter()
                    .filter(|(i, _, _, intra)| *intra && *i != *esc_idx)
                    .map(|(i, _, _, _)| *i)
                    .collect();
                let output = if is_let_kind || used_after || captured_and_called {
                    ScopeOutputItem {
                        skip_idx: None,
                        cache_expr: esc_name.clone().unwrap_or_else(|| "undefined".to_string()),
                        out_name: esc_name.clone(),
                        out_kw: "let",
                        is_named_var: true,
                    }
                } else {
                    ScopeOutputItem {
                        skip_idx: Some(*esc_idx),
                        cache_expr: esc_value_expr.clone(),
                        out_name: esc_name.clone(),
                        out_kw: "const",
                        is_named_var: false,
                    }
                };
                return ScopeOutput { outputs: vec![output], intra_scope_stores: esc_intra, terminal_place_id: None, terminal_type_cast_annotation: None };
            }
            // Check if the feed instruction is a LoadLocal of a named variable.
            // If so, use named-var approach: `let ret;` / `ret = ...` / `else { ret = $[N]; }`.
            // This handles loop-wrapped scopes where `ret` is the loop output, loaded from
            // the phi result in the fallthrough block.
            let feed_named_var = instrs.get(feed_idx).and_then(|i| {
                if let InstructionValue::LoadLocal { place, .. } = &i.value {
                    let name = self.env.get_identifier(place.identifier)
                        .and_then(|id| id.name.as_ref())
                        .map(|n| n.value().to_string());
                    name
                } else {
                    None
                }
            });
            if let Some(named_var) = feed_named_var {
                return ScopeOutput {
                    outputs: vec![ScopeOutputItem {
                        skip_idx: Some(feed_idx),  // skip the LoadLocal (the var is used directly)
                        cache_expr: named_var.clone(),
                        out_name: Some(named_var),
                        out_kw: "let",
                        is_named_var: true,
                    }],
                    intra_scope_stores,
                    terminal_place_id: Some(feed_id),
                    terminal_type_cast_annotation: None,
                };
            }
            // If the terminal feed instruction is a TypeCastExpression (e.g. `x as const`),
            // strip the annotation from the scope body expr and propagate it to the return site.
            // This produces `tN = [callback]` in the scope body and `return tN as const`.
            let type_cast_annotation = instrs.get(feed_idx).and_then(|i| {
                if let InstructionValue::TypeCastExpression { value, source_annotation, .. } = &i.value {
                    if let Some(ann) = source_annotation {
                        // Re-compute the inner value expression (without annotation).
                        let inner_expr = instrs.iter()
                            .find(|ii| ii.lvalue.identifier == value.identifier)
                            .and_then(|vi| self.try_inline_instr(vi))
                            .unwrap_or_else(|| self.expr(value));
                        return Some((ann.clone(), inner_expr));
                    }
                }
                None
            });
            let (final_cache_expr, type_cast_ann) = if let Some((ann, inner)) = type_cast_annotation {
                (inner, Some(ann))
            } else {
                (feed_expr, None)
            };
            return ScopeOutput {
                outputs: vec![ScopeOutputItem {
                    skip_idx: Some(feed_idx),
                    cache_expr: final_cache_expr,
                    out_name: None,
                    out_kw: "const",
                    is_named_var: false,
                }],
                intra_scope_stores,
                terminal_place_id: Some(feed_id),
                terminal_type_cast_annotation: type_cast_ann,
            };
        }

        // Collect ALL escaping StoreLocals as outputs (in instruction order).
        // Deduplicate by variable name: if the same named variable has multiple
        // StoreLocals that escape (e.g. `let y = {}` then `y = x` after IIFE inlining),
        // only the LAST one is the true scope output — earlier assignments are overwritten.
        let escaping_raw: Vec<&(usize, Option<String>, String, bool)> = store_local_info.iter()
            .filter(|(_, _, _, intra)| !*intra)
            .collect();
        // For named variables with duplicates, keep only the last occurrence.
        let mut seen_names: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        for (pos, (_, name, _, _)) in escaping_raw.iter().enumerate() {
            if let Some(n) = name {
                seen_names.insert(n.clone(), pos);
            }
        }
        let escaping: Vec<&(usize, Option<String>, String, bool)> = escaping_raw.iter()
            .enumerate()
            .filter(|(pos, (_, name, _, _))| {
                if let Some(n) = name {
                    seen_names.get(n) == Some(pos)
                } else {
                    true // unnamed temps are always kept
                }
            })
            .map(|(_, item)| *item)
            .collect();

        if !escaping.is_empty() {
            let mut outputs: Vec<ScopeOutputItem> = Vec::new();
            for (idx, name, value_expr, _) in &escaping {
                let (var_id, lvalue_kind) = instrs.get(*idx).and_then(|i| {
                    if let InstructionValue::StoreLocal { lvalue, .. } = &i.value {
                        Some((lvalue.place.identifier, lvalue.kind))
                    } else { None }
                }).unzip();
                // Named-var approach if:
                // (a) the variable is used by later instructions in scope (LoadLocal after the store), OR
                // (b) the variable is let-declared — let vars can be reassigned in the else branch
                //     (cache-hit path: `ret = $[0]`), so they must be hoisted as `let ret`.
                //     After SSA, let-var uses may appear only as phi operands (not LoadLocals),
                //     so the LoadLocal scan misses them.
                let is_let_kind = lvalue_kind.map(|k| matches!(k,
                    InstructionKind::Let | InstructionKind::HoistedLet | InstructionKind::Reassign
                )).unwrap_or(false);
                let used_after = var_id.map(|vid| {
                    instrs.iter().skip(*idx + 1).any(|i| {
                        if let InstructionValue::LoadLocal { place, .. } = &i.value {
                            if place.identifier == vid {
                                return *self.use_count.get(&i.lvalue.identifier.0).unwrap_or(&0) > 0;
                            }
                        }
                        false
                    })
                }).unwrap_or(false);
                // Check if this variable is captured by any function expression
                // within the scope. If so, the closure may read/mutate it, and the
                // variable must be declared before the scope (named-var) so it's
                // visible inside the closure body.
                //
                // (c) captured + called: a FunctionExpression in the scope captures
                //     this variable AND a CallExpression also exists in the scope,
                //     meaning the closure may be invoked during the scope and mutate
                //     the variable. In that case, the variable must be visible before
                //     the scope block so the closure body can access it.
                let captured_and_called = var_id.map(|vid| {
                    let var_decl_id = self.env.get_identifier(vid)
                        .map(|i| i.declaration_id);
                    let has_call = instrs.iter().any(|i| matches!(&i.value,
                        InstructionValue::CallExpression { .. } | InstructionValue::MethodCall { .. }
                    ));
                    if !has_call { return false; }
                    instrs.iter().any(|i| {
                        if let InstructionValue::FunctionExpression { lowered_func, .. }
                            | InstructionValue::ObjectMethod { lowered_func, .. } = &i.value
                        {
                            lowered_func.func.context.iter().any(|ctx| {
                                if let Some(d) = var_decl_id {
                                    self.env.get_identifier(ctx.identifier)
                                        .map(|ci| ci.declaration_id == d)
                                        .unwrap_or(false)
                                } else {
                                    ctx.identifier == vid
                                }
                            })
                        } else {
                            false
                        }
                    })
                }).unwrap_or(false);
                let is_named_var = is_let_kind || used_after || captured_and_called;
                if std::env::var("RC_DEBUG").is_ok() {
                    eprintln!("[analyze_scope] StoreLocal idx={} name={:?} used_after={} is_let_kind={} is_named_var={} instrs.len()={}",
                        idx, name, used_after, is_let_kind, is_named_var, instrs.len());
                }
                if is_named_var {
                    outputs.push(ScopeOutputItem {
                        skip_idx: None,
                        cache_expr: name.clone().unwrap_or_else(|| "undefined".to_string()),
                        out_name: name.clone(),
                        out_kw: "let",
                        is_named_var: true,
                    });
                } else {
                    outputs.push(ScopeOutputItem {
                        skip_idx: Some(*idx),
                        cache_expr: value_expr.clone(),
                        out_name: name.clone(),
                        out_kw: "const",
                        is_named_var: false,
                    });
                }
            }
            let esc_indices: std::collections::HashSet<usize> = escaping.iter().map(|(i, _, _, _)| *i).collect();
            // Shadowed-out escaping stores: earlier StoreLocals for the same named variable
            // that were deduplicated (not in `escaping` anymore). Treat them as intra-scope
            // stores so they still get emitted as assignments inside the scope block.
            let shadowed_esc_indices: std::collections::HashSet<usize> = escaping_raw.iter()
                .enumerate()
                .filter(|(pos, (idx, name, _, _))| {
                    !esc_indices.contains(idx) && name.as_ref().map_or(false, |n| seen_names.contains_key(n))
                })
                .map(|(_, (idx, _, _, _))| *idx)
                .collect();
            let intra: Vec<usize> = store_local_info.iter()
                .filter(|(i, _, _, intra)| (*intra || shadowed_esc_indices.contains(i)) && !esc_indices.contains(i))
                .map(|(i, _, _, _)| *i)
                .collect();
            return ScopeOutput { outputs, intra_scope_stores: intra, terminal_place_id: None, terminal_type_cast_annotation: None };
        }

        // No StoreLocal found. Collect ALL non-transparent instructions whose lvalue
        // is used outside the scope (multi-output support for FunctionExpression + ArrayExpression etc.).
        let mut multi_outputs: Vec<ScopeOutputItem> = Vec::new();
        for (idx, instr) in instrs.iter().enumerate() {
            let is_transparent = matches!(&instr.value,
                InstructionValue::LoadLocal { .. }
                | InstructionValue::LoadGlobal { .. }
                | InstructionValue::LoadContext { .. }
                | InstructionValue::PropertyLoad { .. }
            );
            if is_transparent { continue; }
            let uses = *self.use_count.get(&instr.lvalue.identifier.0).unwrap_or(&0);
            if uses == 0 { continue; }
            let used_outside = self.is_var_used_outside_scope(instr.lvalue.identifier, &scope_lvalue_ids);
            if !used_outside { continue; }
            // Prefer try_inline_instr (fresh at emission time, uses current inlined_exprs
            // with post-scope-emission codegen names) over the potentially-stale
            // build_inline_map entry.
            let cache_expr = if let Some(computed) = self.try_inline_instr(instr) {
                computed
            } else if let Some(inlined) = self.inlined_exprs.get(&instr.lvalue.identifier.0) {
                inlined.clone()
            } else {
                self.expr(&instr.lvalue)
            };
            multi_outputs.push(ScopeOutputItem {
                skip_idx: Some(idx),
                cache_expr,
                out_name: None,
                out_kw: "const",
                is_named_var: false,
            });
        }
        if !multi_outputs.is_empty() {
            return ScopeOutput {
                outputs: multi_outputs,
                intra_scope_stores,
                terminal_place_id: None,
                terminal_type_cast_annotation: None,
            };
        }
        // Fallback: last non-transparent instruction that is used OUTSIDE the scope.
        for (idx, instr) in instrs.iter().enumerate().rev() {
            let is_transparent = matches!(&instr.value,
                InstructionValue::LoadLocal { .. }
                | InstructionValue::LoadGlobal { .. }
                | InstructionValue::LoadContext { .. }
                | InstructionValue::PropertyLoad { .. }
            );
            if is_transparent { continue; }
            let uses = *self.use_count.get(&instr.lvalue.identifier.0).unwrap_or(&0);
            if uses == 0 { continue; }
            // Only emit as scope output if the value is actually consumed outside this scope.
            if !self.is_var_used_outside_scope(instr.lvalue.identifier, &scope_lvalue_ids) { continue; }
            // Prefer fresh try_inline_instr over stale build_inline_map entry.
            let cache_expr = if let Some(computed) = self.try_inline_instr(instr) {
                computed
            } else if let Some(inlined) = self.inlined_exprs.get(&instr.lvalue.identifier.0) {
                inlined.clone()
            } else {
                self.expr(&instr.lvalue)
            };
            return ScopeOutput {
                outputs: vec![ScopeOutputItem {
                    skip_idx: Some(idx),
                    cache_expr,
                    out_name: None,
                    out_kw: "const",
                    is_named_var: false,
                }],
                intra_scope_stores,
                terminal_place_id: None,
                terminal_type_cast_annotation: None,
            };
        }

        // Last-resort fallback: emit the last non-transparent instruction in the scope
        // even if its use_count is 0. This handles cases where the value is consumed
        // by an opaque instruction (InlineJs) that doesn't declare its operands in the
        // HIR visitor, causing use_count to appear zero.
        for (idx, instr) in instrs.iter().enumerate().rev() {
            let is_transparent = matches!(&instr.value,
                InstructionValue::LoadLocal { .. }
                | InstructionValue::LoadGlobal { .. }
                | InstructionValue::LoadContext { .. }
                | InstructionValue::PropertyLoad { .. }
            );
            if is_transparent { continue; }
            let cache_expr = if let Some(computed) = self.try_inline_instr(instr) {
                computed
            } else if let Some(inlined) = self.inlined_exprs.get(&instr.lvalue.identifier.0) {
                inlined.clone()
            } else {
                self.expr(&instr.lvalue)
            };
            return ScopeOutput {
                outputs: vec![ScopeOutputItem {
                    skip_idx: Some(idx),
                    cache_expr,
                    out_name: None,
                    out_kw: "const",
                    is_named_var: false,
                }],
                intra_scope_stores,
                terminal_place_id: None,
                terminal_type_cast_annotation: None,
            };
        }

        ScopeOutput {
            outputs: vec![ScopeOutputItem {
                skip_idx: None,
                cache_expr: "undefined".to_string(),
                out_name: None,
                out_kw: "const",
                is_named_var: false,
            }],
            intra_scope_stores,
            terminal_place_id: None,
            terminal_type_cast_annotation: None,
        }
    }

    /// Checks if a `let`-declared variable escapes through the phi chain.
    ///
    /// After SSA, a `let` variable written in the scope entry block keeps its
    /// original pre-SSA identifier in `StoreLocal.lvalue.place`. When the loop
    /// body has a phi `phi(var_id=original, ...)`, the phi result propagates
    /// the value through the loop. If that phi result is used OUTSIDE the scope
    /// (e.g., in the return terminal), the variable effectively escapes.
    ///
    /// We detect this by:
    /// 1. Finding all phi nodes whose operands include `var_id`.
    /// 2. Checking if any such phi's result is used outside the scope.
    fn is_let_var_phi_escaped(
        &self,
        var_id: crate::hir::hir::IdentifierId,
        scope_lvalue_ids: &std::collections::HashSet<u32>,
    ) -> bool {
        let mut visited = std::collections::HashSet::new();
        self.is_let_var_phi_escaped_inner(var_id, scope_lvalue_ids, &mut visited)
    }

    fn is_let_var_phi_escaped_inner(
        &self,
        var_id: crate::hir::hir::IdentifierId,
        scope_lvalue_ids: &std::collections::HashSet<u32>,
        visited: &mut std::collections::HashSet<u32>,
    ) -> bool {
        if !visited.insert(var_id.0) {
            return false; // Already checked this var — break cycle
        }
        for (_, block) in &self.hir.body.blocks {
            for phi in &block.phis {
                let has_var_operand = phi.operands.values().any(|op| op.identifier == var_id);
                if !has_var_operand {
                    continue;
                }
                let phi_result_id = phi.place.identifier;
                if self.is_var_used_outside_scope_inner(phi_result_id, scope_lvalue_ids, &mut visited.clone()) {
                    return true;
                }
                if self.is_let_var_phi_escaped_inner(phi_result_id, scope_lvalue_ids, visited) {
                    return true;
                }
            }
        }
        false
    }

    /// Checks if a named variable (by its identifier id) is used in any instruction
    /// OUTSIDE the given set of scope instruction lvalue ids.
    ///
    /// Also transitively follows LoadLocal chains: if a LoadLocal inside the scope
    /// reads var_id, and THAT LoadLocal's result is used outside the scope, the
    /// variable is considered to escape (e.g., `fnResult` loaded inside scope whose
    /// result feeds the return terminal).
    fn is_var_used_outside_scope(
        &self,
        var_id: crate::hir::hir::IdentifierId,
        scope_lvalue_ids: &std::collections::HashSet<u32>,
    ) -> bool {
        let mut visited = std::collections::HashSet::new();
        self.is_var_used_outside_scope_inner(var_id, scope_lvalue_ids, &mut visited)
    }

    fn is_var_used_outside_scope_inner(
        &self,
        var_id: crate::hir::hir::IdentifierId,
        scope_lvalue_ids: &std::collections::HashSet<u32>,
        visited: &mut std::collections::HashSet<u32>,
    ) -> bool {
        if !visited.insert(var_id.0) {
            return false; // Already checked — break cycle
        }
        // InlineJs instructions reference variables by name, not by HIR operand.
        // If var_id is a named variable referenced in any InlineJs source, treat it as used outside.
        if self.inline_js_referenced_ids.contains(&var_id) {
            return true;
        }
        use crate::hir::visitors::each_instruction_value_operand;
        use crate::hir::visitors::each_terminal_operand;
        for (_, block) in &self.hir.body.blocks {
            for instr in &block.instructions {
                let in_scope = scope_lvalue_ids.contains(&instr.lvalue.identifier.0);
                if !in_scope {
                    // Instruction is OUTSIDE the scope — check if it directly uses var_id.
                    for op in each_instruction_value_operand(&instr.value) {
                        if op.identifier == var_id {
                            return true;
                        }
                    }
                    // Also check StoreLocal target (the stored-to variable).
                    if let InstructionValue::StoreLocal { lvalue, .. } = &instr.value {
                        if lvalue.place.identifier == var_id {
                            return true;
                        }
                    }
                } else {
                    // Instruction is INSIDE the scope — if it's a LoadLocal of var_id,
                    // check if its RESULT is used outside the scope.
                    if let InstructionValue::LoadLocal { place, .. } = &instr.value {
                        if place.identifier == var_id {
                            let load_result_id = instr.lvalue.identifier;
                            if self.is_var_used_outside_scope_inner(load_result_id, scope_lvalue_ids, visited) {
                                return true;
                            }
                        }
                    }
                }
            }
            // Check terminal.
            for op in each_terminal_operand(&block.terminal) {
                if op.identifier == var_id {
                    return true;
                }
            }
        }
        false
    }

    /// Find an instruction in `instrs` that is:
    /// 1. In `inlined_exprs` (inlined)
    /// 2. Used in the return terminal
    ///
    /// Returns (idx_in_instrs, instr_lvalue_id, inlined_expr_string) or None.
    fn find_terminal_feed_instr(
        &self,
        instrs: &[&Instruction],
    ) -> Option<(usize, u32, String)> {
        // Find the return terminal's place.
        let terminal_place_id = self.collect_terminal_return_place_id()?;

        // Check if terminal_place_id is in inlined_exprs (it's an inlined temp).
        // The terminal directly uses this temp.
        if let Some(inlined_expr) = self.inlined_exprs.get(&terminal_place_id) {
            // Find the instruction in scope that produces this temp.
            if let Some((idx, prod_instr)) = instrs.iter().enumerate()
                .find(|(_, i)| i.lvalue.identifier.0 == terminal_place_id)
            {
                // Prefer try_inline_instr (fresh, uses current inlined_exprs after promotion)
                // over the stale build_inline_map entry in inlined_exprs.
                let fresh_expr = self.try_inline_instr(prod_instr)
                    .unwrap_or_else(|| inlined_expr.clone());
                return Some((idx, terminal_place_id, fresh_expr));
            }
        }

        // The terminal might also use an inlined expr that itself references inlined instrs.
        // Check if any instruction in scope that IS inlined transitively feeds the terminal.
        // For simplicity, check if any inlined instruction in scope has a chain to terminal_place_id.
        for (idx, instr) in instrs.iter().enumerate() {
            let id = instr.lvalue.identifier.0;
            if self.inlined_exprs.contains_key(&id) {
                // Check if this instruction's value is used by the terminal's inlined chain.
                // Simple check: is this instruction's lvalue referenced in the terminal's inlined_expr?
                // We check if `terminal_place_id` ultimately derives from this instruction.
                if self.terminal_derives_from(terminal_place_id, id) {
                    // Find the instruction that produces terminal_place_id (may be in scope or not).
                    // Prefer try_inline_instr for fresh expression over stale inlined_exprs entry.
                    let feed_expr = instrs.iter()
                        .find(|i| i.lvalue.identifier.0 == terminal_place_id)
                        .and_then(|ti| self.try_inline_instr(ti))
                        .or_else(|| self.inlined_exprs.get(&terminal_place_id).cloned())
                        .unwrap_or_else(|| self.inlined_exprs.get(&id).cloned().unwrap_or_default());
                    return Some((idx, terminal_place_id, feed_expr));
                }
            }
        }

        None
    }

    /// Returns the identifier id of the return terminal's value, if the terminal is a Return.
    fn collect_terminal_return_place_id(&self) -> Option<u32> {
        for (_, block) in self.hir.body.blocks.iter().rev() {
            if let Terminal::Return { value, .. } = &block.terminal {
                return Some(value.identifier.0);
            }
        }
        None
    }

    /// Returns true if `target_id` transitively derives from `source_id` through inlined_exprs chains.
    /// Does NOT traverse through scoped identifiers — each scope must claim its own terminal feed.
    /// This prevents scope A from "claiming" a terminal value that goes through scope B's instruction.
    fn terminal_derives_from(&self, target_id: u32, source_id: u32) -> bool {
        if target_id == source_id {
            return true;
        }
        // Don't cross scope boundaries. If target_id belongs to any scope, it is owned by that scope
        // and cannot be claimed as a terminal feed for a different scope's analysis.
        if self.env.get_identifier(crate::hir::hir::IdentifierId(target_id))
            .and_then(|i| i.scope).is_some()
        {
            return false;
        }
        // Check: does the instruction producing target_id use source_id as an operand?
        if let Some(instr) = self.instr_map.get(&target_id) {
            use crate::hir::visitors::each_instruction_value_operand;
            for op in each_instruction_value_operand(&instr.value) {
                if op.identifier.0 == source_id {
                    return true;
                }
                if self.inlined_exprs.contains_key(&op.identifier.0) {
                    if self.terminal_derives_from(op.identifier.0, source_id) {
                        return true;
                    }
                }
            }
        }
        false
    }

    /// Is `instr` the StoreLocal at the end of a scope (whose output goes to tN)?
    fn is_scope_terminal_store(instr: &Instruction) -> bool {
        matches!(&instr.value, InstructionValue::StoreLocal { .. })
    }

    // -----------------------------------------------------------------------
    // Instruction emission
    // -----------------------------------------------------------------------

    fn emit_stmt(
        &self,
        instr: &Instruction,
        _scope_id: Option<ScopeId>,
        scope_out_names: &[String],
    ) -> Option<String> {
        match &instr.value {
            // Transparent — handled via inlined_exprs.
            InstructionValue::LoadGlobal { .. }
            | InstructionValue::LoadLocal { .. }
            | InstructionValue::LoadContext { .. }
            | InstructionValue::PropertyLoad { .. } => None,

            InstructionValue::Primitive { value, .. } => {
                let is_named = self.env.get_identifier(instr.lvalue.identifier)
                    .and_then(|i| i.name.as_ref()).is_some();
                if !is_named && self.inlined_exprs.contains_key(&instr.lvalue.identifier.0) {
                    return None;
                }
                let lv = self.lvalue_name(&instr.lvalue);
                let p = primitive_expr(value);
                Some(format!("const {lv} = {p};"))
            }

            InstructionValue::DeclareLocal { lvalue, .. } => {
                if let Some(name) = self.env.get_identifier(lvalue.place.identifier)
                    .and_then(|i| i.name.as_ref())
                    .map(|n| n.value().to_string())
                {
                    Some(format!("let {name};"))
                } else {
                    None
                }
            }

            InstructionValue::DeclareContext { lvalue, .. } => {
                if let Some(name) = self.env.get_identifier(lvalue.place.identifier)
                    .and_then(|i| i.name.as_ref())
                    .map(|n| n.value().to_string())
                {
                    Some(format!("let {name};"))
                } else {
                    None
                }
            }

            InstructionValue::StoreLocal { lvalue, value, .. } => {
                // Use ident_name() so that name_overrides (shadowing renaming) is applied.
                let raw_name = self.env.get_identifier(lvalue.place.identifier)
                    .and_then(|i| i.name.as_ref())
                    .map(|_| ()); // just check if named
                let name: Option<String> = if raw_name.is_some() {
                    Some(self.ident_name(lvalue.place.identifier))
                } else {
                    None
                };
                let val_expr = self.expr(value);
                // Pure reassignment.
                if let InstructionKind::Reassign = lvalue.kind {
                    if let Some(n) = name {
                        // Skip no-op self-assignments (x = x).
                        if n == val_expr || val_expr == format!("{n};") {
                            return None;
                        }
                        return Some(format!("{n} = {val_expr};"));
                    }
                    return None;
                }

                // If this variable is any scope output (declared with `let` before the if-block),
                // emit as assignment (not declaration). Skip no-op self-assignments.
                if let Some(n) = &name {
                    if scope_out_names.contains(n) {
                        if n.as_str() == val_expr.as_str() {
                            return None; // skip x = x
                        }
                        return Some(format!("{n} = {val_expr};"));
                    }
                }

                let kw = match lvalue.kind {
                    InstructionKind::Const | InstructionKind::HoistedConst | InstructionKind::Function | InstructionKind::HoistedFunction => "const",
                    _ => "let",
                };
                if let Some(n) = name {
                    // Skip self-assignments like `const _temp = _temp`.
                    if n == val_expr {
                        return None;
                    }
                    Some(format!("{kw} {n} = {val_expr};"))
                } else {
                    None
                }
            }

            InstructionValue::CallExpression { callee, args, .. } => {
                let callee_expr = self.expr(callee);
                // If this is an IIFE with no args, unwrap to just the body expression.
                if args.is_empty() {
                    if let Some(inner) = extract_iife_return_expr(&callee_expr) {
                        let name = self.env.get_identifier(instr.lvalue.identifier)
                            .and_then(|i| i.name.as_ref())
                            .map(|n| n.value().to_string());
                        return if let Some(n) = name {
                            Some(format!("const {n} = {inner};"))
                        } else {
                            let lv = self.lvalue_name(&instr.lvalue);
                            Some(format!("{lv} = {inner};"))
                        };
                    }
                }
                // If the callee is an arrow function (contains '=>'), wrap it in parens
                // so that `((x) => expr)()` rather than `(x) => expr()`.
                let callee_str = if callee_expr.contains("=>") {
                    format!("({callee_expr})")
                } else {
                    callee_expr
                };
                let call = format!("{}({})", callee_str, self.call_args(args));
                let name = self.env.get_identifier(instr.lvalue.identifier)
                    .and_then(|i| i.name.as_ref())
                    .map(|n| n.value().to_string());
                let uses = *self.use_count.get(&instr.lvalue.identifier.0).unwrap_or(&0);
                if let Some(n) = name {
                    Some(format!("const {n} = {call};"))
                } else if self.inlined_exprs.contains_key(&instr.lvalue.identifier.0) {
                    None // fully inlined
                } else if uses == 0 {
                    // Result never used — emit as plain statement (e.g. mutate(x);)
                    Some(format!("{call};"))
                } else {
                    // Unnamed temp that couldn't be inlined — emit with temp name.
                    let lv = self.ident_name(instr.lvalue.identifier);
                    Some(format!("const {lv} = {call};"))
                }
            }

            InstructionValue::MethodCall { receiver, property, args, .. } => {
                let recv = self.expr(receiver);
                let method_suffix = self.method_suffix_from_place(property);
                let call = format!("{recv}{method_suffix}({})", self.call_args(args));
                Some(format!("{call};"))
            }

            InstructionValue::NewExpression { callee, args, .. } => {
                let call = format!("new {}({})", self.expr(callee), self.call_args(args));
                let uses = *self.use_count.get(&instr.lvalue.identifier.0).unwrap_or(&0);
                if uses == 0 {
                    Some(format!("{call};"))
                } else {
                    let lv = self.lvalue_name(&instr.lvalue);
                    Some(format!("const {lv} = {call};"))
                }
            }

            InstructionValue::ArrayExpression { elements, .. } => {
                let elems = elements.iter().map(|e| match e {
                    ArrayElement::Place(p) => self.expr(p),
                    ArrayElement::Spread(s) => format!("...{}", self.expr(&s.place)),
                    ArrayElement::Hole => String::new(),
                }).collect::<Vec<_>>().join(", ");
                if self.inlined_exprs.contains_key(&instr.lvalue.identifier.0) {
                    return None;
                }
                let lv = self.lvalue_name(&instr.lvalue);
                Some(format!("const {lv} = [{elems}];"))
            }

            InstructionValue::ObjectExpression { properties, .. } => {
                let props = properties.iter().map(|p| match p {
                    ObjectExpressionProperty::Property(op) => {
                        self.emit_object_property(op)
                    }
                    ObjectExpressionProperty::Spread(s) => format!("...{}", self.expr(&s.place)),
                }).collect::<Vec<_>>().join(", ");
                if self.inlined_exprs.contains_key(&instr.lvalue.identifier.0) {
                    return None;
                }
                let lv = self.lvalue_name(&instr.lvalue);
                if props.is_empty() {
                    Some(format!("const {lv} = {{}};"))
                } else {
                    Some(format!("const {lv} = {{ {props} }};"))
                }
            }

            InstructionValue::BinaryExpression { operator, left, right, .. } => {
                if self.inlined_exprs.contains_key(&instr.lvalue.identifier.0) {
                    return None;
                }
                let op = binary_op_str(operator);
                let lv = self.lvalue_name(&instr.lvalue);
                Some(format!("const {lv} = {} {op} {};", self.expr(left), self.expr(right)))
            }

            InstructionValue::TernaryExpression { test, consequent, alternate, .. } => {
                if self.inlined_exprs.contains_key(&instr.lvalue.identifier.0) {
                    return None;
                }
                let lv = self.lvalue_name(&instr.lvalue);
                Some(format!("const {lv} = {} ? {} : {};", self.expr(test), self.expr(consequent), self.expr(alternate)))
            }

            InstructionValue::UnaryExpression { operator, value, .. } => {
                if self.inlined_exprs.contains_key(&instr.lvalue.identifier.0) {
                    return None;
                }
                let op = unary_op_prefix(operator);
                let lv = self.lvalue_name(&instr.lvalue);
                Some(format!("const {lv} = {op}{};", self.expr(value)))
            }

            InstructionValue::PropertyStore { object, property, value, .. } => {
                Some(format!("{}.{property} = {};", self.expr(object), self.expr(value)))
            }

            InstructionValue::PropertyDelete { object, property, .. } => {
                Some(format!("delete {}.{property};", self.expr(object)))
            }

            InstructionValue::ComputedLoad { object, property, .. } => {
                if self.inlined_exprs.contains_key(&instr.lvalue.identifier.0) {
                    return None;
                }
                let lv = self.lvalue_name(&instr.lvalue);
                let obj = self.expr(object);
                let prop = self.expr(property);
                if let Some(key) = extract_string_literal(&prop) {
                    if is_valid_identifier(&key) {
                        return Some(format!("const {lv} = {obj}.{key};"));
                    }
                }
                Some(format!("const {lv} = {obj}[{prop}];"))
            }

            InstructionValue::ComputedStore { object, property, value, .. } => {
                let obj = self.expr(object);
                let prop = self.expr(property);
                let val = self.expr(value);
                if let Some(key) = extract_string_literal(&prop) {
                    if is_valid_identifier(&key) {
                        return Some(format!("{obj}.{key} = {val};"));
                    }
                }
                Some(format!("{obj}[{prop}] = {val};"))
            }

            InstructionValue::ComputedDelete { object, property, .. } => {
                let obj = self.expr(object);
                let prop = self.expr(property);
                if let Some(key) = extract_string_literal(&prop) {
                    if is_valid_identifier(&key) {
                        return Some(format!("delete {obj}.{key};"));
                    }
                }
                Some(format!("delete {obj}[{prop}];"))
            }

            InstructionValue::JsxExpression { tag, props, children, .. } => {
                if self.inlined_exprs.contains_key(&instr.lvalue.identifier.0) {
                    return None;
                }
                let tag_str = match tag {
                    JsxTag::Builtin(b) => b.name.clone(),
                    JsxTag::Place(p) => self.expr(p),
                };
                let attr_parts: Vec<String> = props.iter().map(|attr| match attr {
                    JsxAttribute::Attribute { name, place } => {
                        let val = self.expr(place);
                        // If the value is a double-quoted JS string literal that contains an
                        // escaped double-quote (`\"`), emit as a single-quoted JSX expression
                        // to match React compiler / Babel output.
                        // e.g. "Some \"text\"" → {'Some "text"'}
                        if val.starts_with('"') && val.ends_with('"') && val.len() >= 2 {
                            let inner = &val[1..val.len()-1];
                            // `inner` is the body of a double-quoted JS string:
                            //   - `\"` means the original string had a double-quote character.
                            //   - `\\` means the original string had a backslash.
                            // We trigger the single-quote conversion if `inner` contains `\"`.
                            if inner.contains("\\\"") {
                                // Convert from double-quoted body to single-quoted body:
                                // `\"` → `"` (unescaped in single-quote context)
                                // `'` → `\'` (must escape single-quotes)
                                // All other escape sequences kept as-is.
                                let mut result = String::with_capacity(inner.len());
                                let bytes = inner.as_bytes();
                                let mut i = 0;
                                while i < bytes.len() {
                                    if bytes[i] == b'\\' && i + 1 < bytes.len() {
                                        if bytes[i + 1] == b'"' {
                                            // `\"` → `"` in single-quoted string
                                            result.push('"');
                                            i += 2;
                                        } else {
                                            // Other escape sequence: keep as-is
                                            result.push('\\');
                                            result.push(bytes[i + 1] as char);
                                            i += 2;
                                        }
                                    } else if bytes[i] == b'\'' {
                                        result.push_str("\\'");
                                        i += 1;
                                    } else {
                                        result.push(bytes[i] as char);
                                        i += 1;
                                    }
                                }
                                format!("{name}={{'{result}'}}")
                            } else {
                                format!("{name}={val}")
                            }
                        } else {
                            format!("{name}={{{val}}}")
                        }
                    }
                    JsxAttribute::Spread { argument } => format!("{{...{}}}", self.expr(argument)),
                }).collect();
                let lv = self.lvalue_name(&instr.lvalue);
                let jsx = match children {
                    Some(ch) if !ch.is_empty() => {
                        let c = ch.iter().map(|c| self.jsx_child_expr(c)).collect::<Vec<_>>().join("");
                        if attr_parts.is_empty() {
                            format!("<{tag_str}>{c}</{tag_str}>")
                        } else {
                            format!("<{tag_str} {}>{c}</{tag_str}>", attr_parts.join(" "))
                        }
                    }
                    _ => {
                        if attr_parts.is_empty() {
                            format!("<{tag_str} />")
                        } else {
                            format!("<{tag_str} {} />", attr_parts.join(" "))
                        }
                    }
                };
                Some(format!("const {lv} = {jsx};"))
            }

            InstructionValue::JsxFragment { children, .. } => {
                if self.inlined_exprs.contains_key(&instr.lvalue.identifier.0) {
                    return None;
                }
                let lv = self.lvalue_name(&instr.lvalue);
                let ch = children.iter().map(|c| self.jsx_child_expr(c)).collect::<Vec<_>>().join("");
                Some(format!("const {lv} = <>{ch}</>;"))
            }

            InstructionValue::FunctionExpression { name, name_hint, fn_type, lowered_func, .. } => {
                if self.inlined_exprs.contains_key(&instr.lvalue.identifier.0) {
                    return None;
                }
                // If outlined, the function body is emitted separately after the component.
                // The name resolves via ident_name → name_hint, so any LoadLocal of this
                // instruction's lvalue will inline to the hint name. No statement needed.
                if name_hint.is_some() {
                    return None;
                }
                let lv = self.lvalue_name(&instr.lvalue);
                let async_kw = if lowered_func.func.async_ { "async " } else { "" };
                let src = &lowered_func.func.original_source;
                if !src.is_empty() {
                    // Apply capture renames before normalizing.
                    let src = apply_capture_renames(src, &lowered_func.func.context, self.env, &self.name_overrides);
                    let normalized = normalize_fn_body_text(&src);
                    // Rename catch parameters to temp names (t0, t1, ...) to match
                    // the reference compiler's rename_variables behavior.
                    let normalized = rename_catch_params_in_text(&normalized);
                    return Some(format!("const {lv} = {normalized};"));
                }
                let fn_name_str = name.as_deref().unwrap_or("");
                let s = match fn_type {
                    FunctionExpressionType::Arrow => format!("const {lv} = {async_kw}() => {{}};"),
                    _ => format!("const {lv} = {async_kw}function {fn_name_str}() {{}};"),
                };
                Some(s)
            }

            InstructionValue::Await { value, .. } => {
                if self.inlined_exprs.contains_key(&instr.lvalue.identifier.0) {
                    return None;
                }
                let uses = *self.use_count.get(&instr.lvalue.identifier.0).unwrap_or(&0);
                if uses == 0 {
                    return Some(format!("await {};", self.expr(value)));
                }
                let lv = self.lvalue_name(&instr.lvalue);
                Some(format!("const {lv} = await {};", self.expr(value)))
            }

            InstructionValue::TemplateLiteral { quasis, subexprs, .. } => {
                if self.inlined_exprs.contains_key(&instr.lvalue.identifier.0) {
                    return None;
                }
                // Empty template literal `` → ""
                if subexprs.is_empty() && quasis.iter().all(|q| q.raw.is_empty()) {
                    let lv = self.lvalue_name(&instr.lvalue);
                    return Some(format!("const {lv} = \"\";"));
                }
                let mut parts = String::from("`");
                for (i, quasi) in quasis.iter().enumerate() {
                    parts.push_str(&quasi.raw);
                    if i < subexprs.len() {
                        let _ = write!(parts, "${{{}}}", self.expr(&subexprs[i]));
                    }
                }
                parts.push('`');
                let lv = self.lvalue_name(&instr.lvalue);
                Some(format!("const {lv} = {parts};"))
            }

            InstructionValue::TaggedTemplateExpression { tag, quasi, .. } => {
                let lv = self.lvalue_name(&instr.lvalue);
                Some(format!("const {lv} = {}`{}`;", self.expr(tag), quasi.raw))
            }

            InstructionValue::Destructure { lvalue, value, .. } => {
                let val = self.expr(value);
                // Check if any bound variable in the pattern is later reassigned/mutated.
                let any_reassigned = match &lvalue.pattern {
                    Pattern::Array(ap) => ap.items.iter().any(|e| match e {
                        ArrayElement::Place(p) => {
                            self.env.get_identifier(p.identifier)
                                .map(|i| self.reassigned_decl_ids.contains(&i.declaration_id))
                                .unwrap_or(false)
                        }
                        _ => false,
                    }),
                    Pattern::Object(op) => op.properties.iter().any(|p| match p {
                        ObjectPatternProperty::Property(prop) => {
                            self.env.get_identifier(prop.place.identifier)
                                .map(|i| self.reassigned_decl_ids.contains(&i.declaration_id))
                                .unwrap_or(false)
                        }
                        _ => false,
                    }),
                };
                let kw = match lvalue.kind {
                    InstructionKind::Reassign => "",
                    _ if any_reassigned => "let",
                    _ => "const",
                };
                let prefix = if kw.is_empty() { String::new() } else { format!("{kw} ") };
                match &lvalue.pattern {
                    Pattern::Array(ap) => {
                        let mut items: Vec<String> = ap.items.iter().map(|e| match e {
                            ArrayElement::Place(p) => {
                                // Emit as hole if the identifier is unused (use_count == 0).
                                if *self.use_count.get(&p.identifier.0).unwrap_or(&0) == 0 {
                                    String::new()
                                } else {
                                    self.ident_name(p.identifier)
                                }
                            }
                            ArrayElement::Spread(s) => format!("...{}", self.ident_name(s.place.identifier)),
                            ArrayElement::Hole => String::new(),
                        }).collect();
                        // Trim trailing holes/unused slots to avoid trailing commas.
                        while items.last().map_or(false, |s| s.is_empty()) {
                            items.pop();
                        }
                        Some(format!("{}[{}] = {val};", prefix, items.join(", ")))
                    }
                    Pattern::Object(op) => {
                        let props: Vec<String> = op.properties.iter().map(|p| match p {
                            ObjectPatternProperty::Property(prop) => {
                                let key_str = self.obj_key(prop.key.clone());
                                let ident_str = self.ident_name(prop.place.identifier);
                                // Use shorthand { value } when key is an identifier and name matches
                                let is_shorthand = matches!(prop.key, ObjectPropertyKey::Identifier(_))
                                    && key_str == ident_str;
                                if is_shorthand {
                                    key_str
                                } else {
                                    format!("{key_str}: {ident_str}")
                                }
                            }
                            ObjectPatternProperty::Spread(s) => format!("...{}", self.ident_name(s.place.identifier)),
                        }).collect();
                        let props_str = props.join(", ");
                        if props_str.is_empty() {
                            Some(format!("{}{{}} = {val};", prefix))
                        } else {
                            Some(format!("{}{{ {props_str} }} = {val};", prefix))
                        }
                    }
                }
            }

            InstructionValue::PrefixUpdate { lvalue, operation, .. } => {
                let op = update_op_str(operation);
                let expr_str = format!("{}{}", op, self.expr(lvalue));
                // Check if the result is captured into a different variable
                let result_lv = self.lvalue_name(&instr.lvalue);
                let target_name = self.expr(lvalue);
                if result_lv != target_name && !result_lv.starts_with("$t") {
                    Some(format!("const {result_lv} = {expr_str};"))
                } else {
                    Some(format!("{expr_str};"))
                }
            }

            InstructionValue::PostfixUpdate { lvalue, operation, .. } => {
                let op = update_op_str(operation);
                let expr_str = format!("{}{}", self.expr(lvalue), op);
                // Check if the result is captured into a different variable
                let result_lv = self.lvalue_name(&instr.lvalue);
                let target_name = self.expr(lvalue);
                if result_lv != target_name && !result_lv.starts_with("$t") {
                    Some(format!("const {result_lv} = {expr_str};"))
                } else {
                    Some(format!("{expr_str};"))
                }
            }

            InstructionValue::RegExpLiteral { pattern, flags, .. } => {
                let lv = self.lvalue_name(&instr.lvalue);
                Some(format!("const {lv} = /{pattern}/{flags};"))
            }

            InstructionValue::StoreContext { lvalue, value, .. } => {
                Some(format!("{} = {};", self.ident_name(lvalue.place.identifier), self.expr(value)))
            }

            InstructionValue::StoreGlobal { name, value, .. } => {
                Some(format!("{name} = {};", self.expr(value)))
            }

            InstructionValue::TypeCastExpression { value, source_annotation, .. } => {
                if self.inlined_exprs.contains_key(&instr.lvalue.identifier.0) {
                    return None;
                }
                let lv = self.lvalue_name(&instr.lvalue);
                let v = self.expr(value);
                let expr = if let Some(ann) = source_annotation {
                    format!("{v} as {ann}")
                } else {
                    v
                };
                if lv == expr { None } else { Some(format!("const {lv} = {expr};")) }
            }

            InstructionValue::GetIterator { collection, .. } => {
                let lv = self.lvalue_name(&instr.lvalue);
                Some(format!("const {lv} = {}[Symbol.iterator]();", self.expr(collection)))
            }

            InstructionValue::IteratorNext { iterator, .. } => {
                let lv = self.lvalue_name(&instr.lvalue);
                Some(format!("const {lv} = {}.next();", self.expr(iterator)))
            }

            InstructionValue::NextPropertyOf { value, .. } => {
                let lv = self.lvalue_name(&instr.lvalue);
                Some(format!("const {lv} = {};", self.expr(value)))
            }

            InstructionValue::Debugger { .. } => Some("debugger;".to_string()),

            InstructionValue::MetaProperty { meta, property, .. } => {
                let lv = self.lvalue_name(&instr.lvalue);
                Some(format!("const {lv} = {meta}.{property};"))
            }

            InstructionValue::InlineJs { source, .. } => {
                // If already inlined into another expression, produce nothing.
                if self.inlined_exprs.contains_key(&instr.lvalue.identifier.0) {
                    return None;
                }
                // Named: will be emitted via StoreLocal; produce nothing here.
                let is_named = self.env.get_identifier(instr.lvalue.identifier)
                    .and_then(|i| i.name.as_ref()).is_some();
                if is_named {
                    None
                } else {
                    // Unnamed and not inlined — emit as expression statement.
                    Some(format!("{source};"))
                }
            }

            InstructionValue::StartMemoize { .. }
            | InstructionValue::FinishMemoize { .. }
            | InstructionValue::UnsupportedNode { .. }
            | InstructionValue::JsxText { .. }
            | InstructionValue::ObjectMethod { .. } => None,
        }
    }

    // -----------------------------------------------------------------------
    // Terminal emission
    // -----------------------------------------------------------------------

    fn emit_terminal(&self, terminal: &Terminal) -> String {
        match terminal {
            Terminal::Return { value, return_variant, .. } => {
                use crate::hir::hir::ReturnVariant;
                // Void (implicit) returns emit nothing — the JS default is to return
                // undefined when the function falls off the end.
                if matches!(return_variant, ReturnVariant::Void) {
                    // Only skip if the value resolves to `undefined` (not a real value).
                    let expr = if let Some(replacement) = self.terminal_replacement.get(&value.identifier.0) {
                        replacement.clone()
                    } else {
                        self.expr(value)
                    };
                    if expr == "undefined" {
                        return String::new();
                    }
                    return format!("return {expr};");
                }
                // If a scope replaced this terminal's value with a temp, use the temp.
                let expr = if let Some(replacement) = self.terminal_replacement.get(&value.identifier.0) {
                    replacement.clone()
                } else {
                    self.expr(value)
                };
                // `return undefined;` → `return;` to match reference compiler
                if expr == "undefined" {
                    return "return;".to_string();
                }
                format!("return {expr};")
            }
            Terminal::Throw { value, .. } => format!("throw {};", self.expr(value)),
            _ => String::new(),
        }
    }

    // -----------------------------------------------------------------------
    // Expression resolution
    // -----------------------------------------------------------------------

    /// Resolve a Place to its JS expression, using inlined_exprs if available.
    fn expr(&self, place: &Place) -> String {
        if let Some(s) = self.inlined_exprs.get(&place.identifier.0) {
            return s.clone();
        }
        self.ident_name(place.identifier)
    }

    /// Render a reactive scope dependency as a JS expression, including member path.
    /// e.g., `{ place: props, path: [value] }` → `"props.value"`
    fn dep_expr(&self, dep: &ReactiveScopeDependency) -> String {
        // Prefer ident_name (which checks env names + ssa_value_to_name) over inlined_exprs.
        // inlined_exprs may have "foo(props.x)" for an anonymous temp that flows into a named
        // variable; we want "x" (the named variable) as the dep expression, not the expression
        // that computed it. Only fall back to inlined_exprs if there's no proper name.
        let name = self.ident_name(dep.place.identifier);
        let base = if name.starts_with("$t") {
            // No proper name; use inlined_exprs (cache vars from scope emission, etc.)
            if let Some(s) = self.inlined_exprs.get(&dep.place.identifier.0) {
                s.clone()
            } else {
                name
            }
        } else {
            name
        };
        if dep.path.is_empty() {
            base
        } else {
            let path: String = dep.path.iter()
                .map(|entry| {
                    if entry.optional {
                        format!("?.{}", entry.property)
                    } else {
                        format!(".{}", entry.property)
                    }
                })
                .collect();
            format!("{base}{path}")
        }
    }

    fn ident_name(&self, id: crate::hir::hir::IdentifierId) -> String {
        // Check name override first (renamed for shadowing resolution).
        if let Some(name) = self.name_overrides.get(&id.0) {
            return name.clone();
        }
        if let Some(name) = self.env
            .get_identifier(id)
            .and_then(|i| i.name.as_ref())
            .map(|n| n.value().to_string())
        {
            return name;
        }
        // Fall back to ssa_value_to_name for SSA temps that flow into named variables.
        if let Some(name) = self.ssa_value_to_name.get(&id.0) {
            return name.clone();
        }
        // Check scope output temp names (tN assigned during scope emission).
        if let Some(name) = self.scope_output_names.get(&id.0) {
            return name.clone();
        }
        // Check if this identifier is the lvalue of an outlined FunctionExpression.
        if let Some(instr) = self.instr_map.get(&id.0) {
            if let InstructionValue::FunctionExpression { name_hint: Some(hint), .. } = &instr.value {
                return hint.clone();
            }
        }
        format!("$t{}", id.0)
    }

    fn lvalue_name(&self, place: &Place) -> String {
        self.ident_name(place.identifier)
    }

    fn binding_name(&self, binding: &NonLocalBinding) -> String {
        match binding {
            NonLocalBinding::Global { name }
            | NonLocalBinding::ModuleLocal { name }
            | NonLocalBinding::ImportDefault { name, .. }
            | NonLocalBinding::ImportNamespace { name, .. }
            | NonLocalBinding::ImportSpecifier { name, .. } => name.clone(),
        }
    }

    /// For a MethodCall's `property` place: extract just the method name.
    /// The property place is typically produced by a PropertyLoad instruction.
    /// Returns the method accessor suffix for a MethodCall property.
    /// Dot notation: `.methodName`; computed: `[expr]`.
    fn method_suffix_from_place(&self, place: &Place) -> String {
        if let Some(src_instr) = self.instr_map.get(&place.identifier.0) {
            match &src_instr.value {
                InstructionValue::PropertyLoad { property, .. } => {
                    return format!(".{property}");
                }
                InstructionValue::ComputedLoad { property, .. } => {
                    let prop_expr = self.expr(property);
                    // If the computed property is a string literal that's a valid
                    // JS identifier, use dot notation (matches TS compiler behavior
                    // for constant-folded computed properties).
                    if let Some(name) = prop_expr.strip_prefix('"')
                        .and_then(|s| s.strip_suffix('"'))
                    {
                        if !name.is_empty()
                            && name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_' || c == '$')
                            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
                        {
                            return format!(".{name}");
                        }
                    }
                    return format!("[{prop_expr}]");
                }
                _ => {}
            }
        }
        // Fallback: dot notation using inlined expression.
        let expr = self.expr(place);
        if let Some(dot_pos) = expr.rfind('.') {
            format!(".{}", &expr[dot_pos + 1..])
        } else {
            format!(".{expr}")
        }
    }

    /// Emit a JSX child expression. JsxText children are emitted as-is (no braces).
    /// All other expression children are wrapped in `{expr}`.
    fn jsx_child_expr(&self, place: &Place) -> String {
        // Check if this child is a JsxText instruction.
        if let Some(instr) = self.instr_map.get(&place.identifier.0) {
            if let InstructionValue::JsxText { value, .. } = &instr.value {
                // Decode HTML entities in the JSX text.
                let decoded = decode_jsx_html_entities(value);
                // If the decoded text contains JSX-special characters (< { > that could
                // be ambiguous), wrap as a JS string expression: {"decoded"}.
                // The TypeScript/Babel compiler always wraps when entities were present.
                if decoded != *value {
                    // Entities were present: emit as JS string expression.
                    let escaped = decoded.replace('\\', "\\\\").replace('"', "\\\"");
                    return format!("{{\"{}\"}}",  escaped);
                }
                return value.clone();
            }
        }
        // Check if this child is a nested JsxExpression or JsxFragment (no braces needed).
        if let Some(instr) = self.instr_map.get(&place.identifier.0) {
            if matches!(&instr.value,
                InstructionValue::JsxExpression { .. } | InstructionValue::JsxFragment { .. }
            ) {
                return self.expr(place);
            }
        }
        // Expression child: wrap in {}.
        format!("{{{}}}", self.expr(place))
    }

    fn call_arg(&self, arg: &CallArg) -> String {
        match arg {
            CallArg::Place(p) => self.expr(p),
            CallArg::Spread(s) => format!("...{}", self.expr(&s.place)),
        }
    }

    fn call_args(&self, args: &[CallArg]) -> String {
        args.iter().map(|a| self.call_arg(a)).collect::<Vec<_>>().join(", ")
    }

    fn obj_key(&self, key: ObjectPropertyKey) -> String {
        match key {
            ObjectPropertyKey::Identifier(s) => s,
            // String literal keys: quote only if not a valid JS identifier
            // (e.g. "data-foo-bar" → "data-foo-bar", "data" → data).
            ObjectPropertyKey::String(s) => {
                if is_valid_identifier(&s) { s } else { format!("\"{}\"", escape_js_string(&s)) }
            }
            ObjectPropertyKey::Computed(p) => format!("[{}]", self.expr(&p)),
            ObjectPropertyKey::Number(n) => n.to_string(),
        }
    }

    /// Emit an object property, using shorthand notation when key == value name.
    /// E.g., `{ a: a }` → `{ a }` and `{ a: b }` → `{ a: b }`.
    /// For method shorthand `{ method() {} }`, emits `method() {}` not `method: () {}`.
    fn emit_object_property(&self, op: &crate::hir::hir::ObjectProperty) -> String {
        use crate::hir::hir::{ObjectPropertyKey, ObjectPropertyType};
        let val_expr = self.expr(&op.place);
        // Method shorthand: `method() { body }` not `method: () { body }`.
        // The value expression starts with `(` when it's a function expression body
        // (e.g. `() {}`, `(x) { return x; }`). Arrow functions start with `() =>`.
        // For method shorthand, strip the `: ` separator and just prepend the key.
        if matches!(op.type_, ObjectPropertyType::Method) {
            // val_expr is like `() {}` or `(x, y) { ... }` — not an arrow.
            // Emit as `key() {}` (method shorthand form).
            if val_expr.starts_with('(') && !val_expr.contains("=>") {
                let key_str = self.obj_key(op.key.clone());
                return format!("{key_str}{val_expr}");
            }
        }
        // Check if we can use shorthand: key must be an identifier, and value
        // expression must be the same name (a plain variable reference).
        let key_name = match &op.key {
            ObjectPropertyKey::Identifier(s) => Some(s.as_str()),
            _ => None,
        };
        if let Some(key) = key_name {
            if val_expr == key {
                return key.to_string();
            }
        }
        let key_str = self.obj_key(op.key.clone());
        format!("{key_str}: {val_expr}")
    }

    // -----------------------------------------------------------------------
    // Parameters
    // -----------------------------------------------------------------------

    fn emit_params(&self) -> String {
        self.hir.params.iter().map(|p| match p {
            Param::Place(place) => self.ident_name(place.identifier),
            Param::Spread(s) => format!("...{}", self.ident_name(s.place.identifier)),
        }).collect::<Vec<_>>().join(", ")
    }

    // -----------------------------------------------------------------------
    // CFG traversal
    // -----------------------------------------------------------------------

    fn collect_instructions_in_order(&self) -> Vec<Instruction> {
        let mut result = Vec::new();
        let mut visited = std::collections::HashSet::new();
        let mut queue = std::collections::VecDeque::new();
        queue.push_back(self.hir.body.entry);

        while let Some(bid) = queue.pop_front() {
            if !visited.insert(bid) { continue; }
            if let Some(block) = self.hir.body.blocks.get(&bid) {
                for instr in &block.instructions {
                    result.push(instr.clone());
                }
                if let Some(ft) = block.terminal.fallthrough() {
                    queue.push_back(ft);
                }
                for succ in block.terminal.successors() {
                    if Some(succ) != block.terminal.fallthrough() {
                        queue.push_back(succ);
                    }
                }
            }
        }
        result
    }

    fn collect_terminal(&self) -> Terminal {
        for (_, block) in self.hir.body.blocks.iter().rev() {
            if matches!(block.terminal, Terminal::Return { .. }) {
                return block.terminal.clone();
            }
        }
        if let Some((_, block)) = self.hir.body.blocks.iter().last() {
            return block.terminal.clone();
        }
        Terminal::Unreachable {
            id: crate::hir::hir::make_instruction_id(0),
            loc: Default::default(),
        }
    }

    // -----------------------------------------------------------------------
    // Scope assignment
    // -----------------------------------------------------------------------

    fn assign_instructions_to_scopes(
        &self,
        instrs: &[Instruction],
    ) -> HashMap<InstructionId, ScopeId> {
        use std::collections::HashSet;
        let mut map = HashMap::new();

        // Pre-compute set of IdentifierId values that come from outlined FunctionExpressions.
        // An outlined FunctionExpression (name_hint set) is a stable module-level reference —
        // it should not be memoized, and neither should StoreLocal instructions that store it.
        let outlined_fn_ids: HashSet<u32> = instrs.iter()
            .filter_map(|instr| {
                if let InstructionValue::FunctionExpression { name_hint, .. } = &instr.value {
                    if name_hint.is_some() {
                        return Some(instr.lvalue.identifier.0);
                    }
                }
                None
            })
            .collect();

        for instr in instrs {
            // 1. Check the instruction's own lvalue identifier.
            if let Some(ident) = self.env.get_identifier(instr.lvalue.identifier) {
                if let Some(sid) = ident.scope {
                    map.insert(instr.id, sid);
                    continue;
                }
            }
            // 2. For StoreLocal/StoreContext: check the stored VALUE's scope,
            //    or the target variable's scope.
            //    EXCEPT: if the value comes from an outlined FunctionExpression, skip —
            //    outlined functions are stable module-level refs and don't need memoization.
            let store_scope = match &instr.value {
                InstructionValue::StoreLocal { lvalue, value, .. } => {
                    if outlined_fn_ids.contains(&value.identifier.0) {
                        None // Outlined function ref — do not place in a scope.
                    } else {
                        // Check value identifier's scope first.
                        self.env.get_identifier(value.identifier).and_then(|i| i.scope)
                            .or_else(|| self.env.get_identifier(lvalue.place.identifier).and_then(|i| i.scope))
                    }
                }
                InstructionValue::StoreContext { lvalue, value, .. } => {
                    if outlined_fn_ids.contains(&value.identifier.0) {
                        None
                    } else {
                        self.env.get_identifier(value.identifier).and_then(|i| i.scope)
                            .or_else(|| self.env.get_identifier(lvalue.place.identifier).and_then(|i| i.scope))
                    }
                }
                _ => None,
            };
            if let Some(sid) = store_scope {
                map.insert(instr.id, sid);
                continue;
            }
            // 3. Fallback: instruction id within a non-zero scope range.
            // EXCEPT: hook calls must never be placed inside a scope block —
            // they must run unconditionally (React's rules of hooks).
            // EXCEPT: outlined FunctionExpressions (name_hint set) are stable
            // module-level references — they don't allocate and don't need scopes.
            let is_excluded = match &instr.value {
                InstructionValue::CallExpression { callee, .. } => {
                    self.inlined_exprs.get(&callee.identifier.0)
                        .map(|name| name.starts_with("use")
                            && name.len() > 3
                            && name[3..].starts_with(|c: char| c.is_uppercase()))
                        .unwrap_or(false)
                }
                InstructionValue::FunctionExpression { name_hint, .. } => name_hint.is_some(),
                // Also exclude StoreLocal/StoreContext of outlined function results
                // even when they fall through to the range-based assignment (step 3).
                InstructionValue::StoreLocal { value, .. } => outlined_fn_ids.contains(&value.identifier.0),
                InstructionValue::StoreContext { value, .. } => outlined_fn_ids.contains(&value.identifier.0),
                _ => false,
            };
            if !is_excluded {
                for (sid, scope) in &self.env.scopes {
                    let range_nonempty = scope.range.end > scope.range.start;
                    if range_nonempty && instr.id >= scope.range.start && instr.id < scope.range.end {
                        map.entry(instr.id).or_insert(*sid);
                    }
                }
            }
        }
        map
    }
}

// ---------------------------------------------------------------------------
// Free helpers
// ---------------------------------------------------------------------------

fn binary_op_str(op: &BinaryOperator) -> &'static str {
    match op {
        BinaryOperator::Add => "+",
        BinaryOperator::Sub => "-",
        BinaryOperator::Mul => "*",
        BinaryOperator::Div => "/",
        BinaryOperator::Mod => "%",
        BinaryOperator::Exp => "**",
        BinaryOperator::BitAnd => "&",
        BinaryOperator::BitOr => "|",
        BinaryOperator::BitXor => "^",
        BinaryOperator::Shl => "<<",
        BinaryOperator::Shr => ">>",
        BinaryOperator::UShr => ">>>",
        BinaryOperator::Eq => "==",
        BinaryOperator::NEq => "!=",
        BinaryOperator::StrictEq => "===",
        BinaryOperator::StrictNEq => "!==",
        BinaryOperator::Lt => "<",
        BinaryOperator::LtEq => "<=",
        BinaryOperator::Gt => ">",
        BinaryOperator::GtEq => ">=",
        BinaryOperator::In => "in",
        BinaryOperator::Instanceof => "instanceof",
    }
}

fn unary_op_prefix(op: &UnaryOperator) -> &'static str {
    match op {
        UnaryOperator::Not => "!",
        UnaryOperator::Minus => "-",
        UnaryOperator::Plus => "+",
        UnaryOperator::BitNot => "~",
        UnaryOperator::Typeof => "typeof ",
        UnaryOperator::Void => "void ",
    }
}

fn update_op_str(op: &UpdateOperator) -> &'static str {
    match op {
        UpdateOperator::Increment => "++",
        UpdateOperator::Decrement => "--",
    }
}

fn primitive_expr(value: &PrimitiveValue) -> String {
    match value {
        PrimitiveValue::Undefined => "undefined".to_string(),
        PrimitiveValue::Null => "null".to_string(),
        PrimitiveValue::Boolean(b) => b.to_string(),
        PrimitiveValue::Number(n) => {
            if n.fract() == 0.0 && n.abs() < 1e15 {
                format!("{}", *n as i64)
            } else {
                format!("{n}")
            }
        }
        PrimitiveValue::String(s) => format!("\"{}\"", escape_js_string(s)),
    }
}

/// Escape a string for use inside double quotes in JavaScript output.
fn escape_js_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{0008}' => out.push_str("\\b"),
            '\u{000C}' => out.push_str("\\f"),
            '\0' => out.push_str("\\0"),
            c if c < ' ' => {
                // Other control characters: use \xNN
                out.push_str(&format!("\\x{:02x}", c as u32));
            }
            _ => out.push(c),
        }
    }
    out
}

fn collect_instr_operands(instr: &Instruction) -> Vec<crate::hir::hir::IdentifierId> {
    let mut ids = Vec::new();
    use crate::hir::visitors::each_instruction_value_operand;
    for op in each_instruction_value_operand(&instr.value) {
        ids.push(op.identifier);
    }
    ids
}

/// If `expr` is a JS string literal (e.g. `"foo"`), return the string content.
fn extract_string_literal(expr: &str) -> Option<String> {
    let expr = expr.trim();
    if expr.len() >= 2 && expr.starts_with('"') && expr.ends_with('"') {
        // Un-escape simple cases (only handle no-backslash strings for safety).
        let inner = &expr[1..expr.len() - 1];
        if !inner.contains('\\') {
            return Some(inner.to_string());
        }
    }
    if expr.len() >= 2 && expr.starts_with('\'') && expr.ends_with('\'') {
        let inner = &expr[1..expr.len() - 1];
        if !inner.contains('\\') {
            return Some(inner.to_string());
        }
    }
    None
}

/// Wrap a JSX expr in parens when the scope-output assignment line would exceed
/// Babel's default print width (80 chars). Simulates Babel's automatic paren
/// insertion for multi-line JSX assignments in the TS React compiler output.
///
/// The line printed is `"    {cache_var} = {expr};"` (4-space indent).
/// If that is >80 chars AND `expr` is a JSX expression (starts with `<`), wrap.
fn maybe_paren_jsx_scope_output(cache_var: &str, expr: &str) -> String {
    if expr.trim_start().starts_with('<') {
        let line_len = 4 + cache_var.len() + 3 + expr.len() + 1; // "    X = Y;"
        if line_len > 80 {
            return format!("({expr})");
        }
    }
    expr.to_string()
}

/// Returns true if `s` is a valid JS identifier (can be used as dot-notation property).
fn is_valid_identifier(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    let mut chars = s.chars();
    let first = chars.next().unwrap();
    if !first.is_alphabetic() && first != '_' && first != '$' {
        return false;
    }
    chars.all(|c| c.is_alphanumeric() || c == '_' || c == '$')
}

// ---------------------------------------------------------------------------
// Grouping helper
// ---------------------------------------------------------------------------

enum InstrGroup<'a> {
    Unscoped(Vec<&'a Instruction>),
    Scoped(ScopeId, Vec<&'a Instruction>),
}

fn group_by_scope<'a>(
    instrs: &'a [Instruction],
    instr_scope: &HashMap<InstructionId, ScopeId>,
    _inlined_exprs: &HashMap<u32, String>,
) -> Vec<InstrGroup<'a>> {
    let mut groups: Vec<InstrGroup<'a>> = Vec::new();
    // Buffer unscoped instructions until we know if the next scoped instruction
    // continues the same scope.  If it does, absorb them (they are transparent
    // reads/FunctionExpression outlines the scope body needs).
    let mut pending_unscoped: Vec<&'a Instruction> = Vec::new();

    for instr in instrs {
        let scope = instr_scope.get(&instr.id).copied();

        if scope.is_none() {
            pending_unscoped.push(instr);
            continue;
        }

        let sid = scope.unwrap();

        // ID of the most-recent Scoped group (ignore Unscoped groups in between).
        let last_scoped_id = groups.iter().rev().find_map(|g| {
            if let InstrGroup::Scoped(s, _) = g { Some(*s) } else { None }
        });

        if last_scoped_id == Some(sid) {
            // Same scope continues: absorb pending unscoped + this instruction.
            if let Some(InstrGroup::Scoped(_, v)) = groups.iter_mut().rev()
                .find(|g| matches!(g, InstrGroup::Scoped(s, _) if *s == sid))
            {
                v.extend(pending_unscoped.drain(..));
                v.push(instr);
            }
        } else {
            // New scope: flush pending as a separate Unscoped group, then start new Scoped.
            if !pending_unscoped.is_empty() {
                groups.push(InstrGroup::Unscoped(pending_unscoped.drain(..).collect()));
            }
            groups.push(InstrGroup::Scoped(sid, vec![instr]));
        }
    }

    if !pending_unscoped.is_empty() {
        groups.push(InstrGroup::Unscoped(pending_unscoped));
    }

    groups
}

/// Normalize arrow function params: add parens if a single unparenthesized identifier param.
/// `e => ...` → `(e) => ...`
/// `(e) => ...` → `(e) => ...` (already parenthesized)
/// `(a, b) => ...` → unchanged
/// Normalize function body text:
///   1. Single-quoted string literals → double-quoted (e.g. `'foo'` → `"foo"`)
///   2. Computed property accesses with simple string keys → dot notation
///      (e.g. `obj['key']` → `obj.key`)
///
/// This runs on `original_source` text from lowered function expressions so that
/// the output matches the TS compiler's normalization behavior (oxc_codegen always
/// outputs double-quoted strings and converts constant computed props to dot notation).
fn normalize_fn_body_text(src: &str) -> String {
    let bytes = src.as_bytes();
    let mut result = String::with_capacity(src.len());
    let mut i = 0;
    while i < bytes.len() {
        // Skip line comments
        if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'/' {
            while i < bytes.len() && bytes[i] != b'\n' { result.push(bytes[i] as char); i += 1; }
            continue;
        }
        // Skip block comments
        if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            result.push(bytes[i] as char); result.push(bytes[i+1] as char); i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                result.push(bytes[i] as char); i += 1;
            }
            if i + 1 < bytes.len() { result.push(bytes[i] as char); result.push(bytes[i+1] as char); i += 2; }
            continue;
        }
        // Template literal — pass through unchanged
        if bytes[i] == b'`' {
            result.push('`');
            i += 1;
            let mut depth = 1i32;
            while i < bytes.len() && depth > 0 {
                if bytes[i] == b'\\' { result.push(bytes[i] as char); i += 1; if i < bytes.len() { result.push(bytes[i] as char); i += 1; } continue; }
                if bytes[i] == b'`' { depth -= 1; }
                result.push(bytes[i] as char); i += 1;
            }
            continue;
        }
        // Double-quoted string — pass through unchanged
        if bytes[i] == b'"' {
            result.push('"');
            i += 1;
            while i < bytes.len() && bytes[i] != b'"' {
                if bytes[i] == b'\\' { result.push(bytes[i] as char); i += 1; if i < bytes.len() { result.push(bytes[i] as char); i += 1; } continue; }
                result.push(bytes[i] as char); i += 1;
            }
            if i < bytes.len() { result.push('"'); i += 1; }
            continue;
        }
        // Single-quoted string — convert to double-quoted
        if bytes[i] == b'\'' {
            let start = i;
            i += 1;
            let mut content = String::new();
            let mut ok = true;
            while i < bytes.len() && bytes[i] != b'\'' {
                if bytes[i] == b'\\' {
                    i += 1;
                    if i < bytes.len() {
                        match bytes[i] {
                            b'\'' => { content.push('\''); i += 1; } // unescape single quote
                            b'"' => { content.push_str("\\\""); i += 1; } // escape double quote
                            _ => { content.push('\\'); content.push(bytes[i] as char); i += 1; }
                        }
                    }
                    continue;
                }
                if bytes[i] == b'"' { content.push_str("\\\""); i += 1; continue; }
                if bytes[i] == b'\n' { ok = false; break; } // multiline — not a simple string
                content.push(bytes[i] as char); i += 1;
            }
            if ok && i < bytes.len() && bytes[i] == b'\'' {
                i += 1; // consume closing quote
                result.push('"');
                result.push_str(&content);
                result.push('"');
            } else {
                // Not a simple single-quoted string — output as-is
                result.push_str(&src[start..i]);
            }
            continue;
        }
        // Computed property: obj['key'] where key is a simple identifier → obj.key
        // Detect `[` preceded by alphanumeric/`)` (property access), then `'key'` or `"key"`, then `]`
        if bytes[i] == b'[' && i > 0 {
            let prev = bytes[i - 1];
            if prev.is_ascii_alphanumeric() || prev == b'_' || prev == b'$' || prev == b')' || prev == b']' {
                // Try to match ['identifier'] or ["identifier"]
                let j = i + 1;
                if j < bytes.len() && (bytes[j] == b'\'' || bytes[j] == b'"') {
                    let q = bytes[j];
                    let mut k = j + 1;
                    let mut key = String::new();
                    let mut key_ok = true;
                    while k < bytes.len() && bytes[k] != q {
                        if bytes[k] == b'\\' || bytes[k] == b'\n' { key_ok = false; break; }
                        key.push(bytes[k] as char); k += 1;
                    }
                    if key_ok && k < bytes.len() && bytes[k] == q && k + 1 < bytes.len() && bytes[k + 1] == b']' {
                        // Check key is a valid identifier
                        if is_valid_identifier(&key) {
                            // Emit .key instead of ['key']
                            result.push('.');
                            result.push_str(&key);
                            i = k + 2; // skip past closing `]`
                            continue;
                        }
                    }
                }
            }
        }
        result.push(bytes[i] as char);
        i += 1;
    }
    // Promote `let` → `const` for variables that are never reassigned in the body.
    // The TS compiler's rename_variables pass does this for inner function declarations.
    result = promote_let_to_const(&result);
    result
}

/// Rename catch parameters to temp names (t0, t1, ...) in source text.
/// Finds `catch (NAME)` or `catch(NAME)` patterns and renames the identifier + all uses.
/// This mirrors the rename_variables behavior from the TypeScript React compiler.
fn rename_catch_params_in_text(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut result = text.to_string();
    let mut counter = 0u32;
    let catch_keyword = b"catch";
    let mut i = 0;
    let mut catches: Vec<String> = Vec::new();
    while i + 5 < bytes.len() {
        if &bytes[i..i + 5] == catch_keyword {
            // Check word boundary before "catch"
            if i > 0 && {
                let b = bytes[i - 1];
                b.is_ascii_alphanumeric() || b == b'_' || b == b'$'
            } {
                i += 1;
                continue;
            }
            let mut j = i + 5;
            // Skip whitespace
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'(' {
                j += 1;
                // Skip whitespace
                while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                    j += 1;
                }
                // Extract identifier
                let start = j;
                while j < bytes.len() && {
                    let b = bytes[j];
                    b.is_ascii_alphanumeric() || b == b'_' || b == b'$'
                } {
                    j += 1;
                }
                if j > start {
                    let name = std::str::from_utf8(&bytes[start..j]).unwrap_or("").to_string();
                    if !name.is_empty() {
                        catches.push(name);
                    }
                }
            }
        }
        i += 1;
    }
    for name in catches {
        let new_name = format!("t{}", counter);
        counter += 1;
        if name != new_name {
            result = rename_word_in_src(&result, &name, &new_name);
        }
    }
    result
}

/// Promote `let x = ...;` to `const x = ...;` when `x` is never reassigned in `src`.
/// Only promotes `let` declarations with an initializer (not bare `let x;`).
fn promote_let_to_const(src: &str) -> String {
    let bytes = src.as_bytes();
    // First pass: collect all `let` declarations with initializers and their variable names.
    let mut let_decls: Vec<(usize, String)> = Vec::new(); // (byte offset of "let ", varname)
    let mut i = 0;
    while i + 4 < bytes.len() {
        // Skip comments
        if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i+1] == b'/' {
            while i < bytes.len() && bytes[i] != b'\n' { i += 1; }
            continue;
        }
        if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i+1] == b'*' {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i+1] == b'/') { i += 1; }
            if i + 1 < bytes.len() { i += 2; }
            continue;
        }
        // Skip strings
        if bytes[i] == b'\'' || bytes[i] == b'"' || bytes[i] == b'`' {
            let q = bytes[i];
            i += 1;
            while i < bytes.len() && bytes[i] != q {
                if bytes[i] == b'\\' { i += 1; }
                i += 1;
            }
            if i < bytes.len() { i += 1; }
            continue;
        }
        if &bytes[i..i+4] == b"let " {
            // Check word boundary before "let"
            if i > 0 && (bytes[i-1].is_ascii_alphanumeric() || bytes[i-1] == b'_' || bytes[i-1] == b'$') {
                i += 1;
                continue;
            }
            let decl_start = i;
            let mut j = i + 4;
            // Skip whitespace
            while j < bytes.len() && bytes[j].is_ascii_whitespace() { j += 1; }
            // Read identifier
            let id_start = j;
            while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_' || bytes[j] == b'$') { j += 1; }
            if j > id_start {
                let name = std::str::from_utf8(&bytes[id_start..j]).unwrap_or("").to_string();
                // Check that next non-ws char is '=' (initializer) and not '=='
                let mut k = j;
                while k < bytes.len() && bytes[k].is_ascii_whitespace() { k += 1; }
                if k < bytes.len() && bytes[k] == b'=' && (k + 1 >= bytes.len() || bytes[k+1] != b'=') {
                    if !name.is_empty() {
                        let_decls.push((decl_start, name));
                    }
                }
            }
            i = j;
            continue;
        }
        i += 1;
    }

    if let_decls.is_empty() {
        return src.to_string();
    }

    // Second pass: check which declared names are reassigned (name = ... but not ==)
    let mut reassigned: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for (_, name) in &let_decls {
        let name_bytes = name.as_bytes();
        let mut i = 0;
        while i + name_bytes.len() < bytes.len() {
            // Skip comments
            if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i+1] == b'/' {
                while i < bytes.len() && bytes[i] != b'\n' { i += 1; }
                continue;
            }
            if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i+1] == b'*' {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i+1] == b'/') { i += 1; }
                if i + 1 < bytes.len() { i += 2; }
                continue;
            }
            // Skip strings
            if bytes[i] == b'\'' || bytes[i] == b'"' || bytes[i] == b'`' {
                let q = bytes[i];
                i += 1;
                while i < bytes.len() && bytes[i] != q {
                    if bytes[i] == b'\\' { i += 1; }
                    i += 1;
                }
                if i < bytes.len() { i += 1; }
                continue;
            }
            if bytes[i..].starts_with(name_bytes) {
                let before_ok = i == 0 || !(bytes[i-1].is_ascii_alphanumeric() || bytes[i-1] == b'_' || bytes[i-1] == b'$');
                let after_pos = i + name_bytes.len();
                let after_ok = after_pos >= bytes.len() || !(bytes[after_pos].is_ascii_alphanumeric() || bytes[after_pos] == b'_' || bytes[after_pos] == b'$');
                if before_ok && after_ok {
                    // Check if this is an assignment (not part of a `let` declaration)
                    let mut k = after_pos;
                    while k < bytes.len() && bytes[k].is_ascii_whitespace() { k += 1; }
                    if k < bytes.len() && bytes[k] == b'=' && (k + 1 >= bytes.len() || bytes[k+1] != b'=') {
                        // Check it's not the original declaration (preceded by "let ")
                        let prefix_check = if i >= 4 { &bytes[i-4..i] } else { b"" };
                        let prefix_check2 = if i >= 6 { &bytes[i-6..i] } else { b"" };
                        if !prefix_check.ends_with(b"let ") && !prefix_check2.ends_with(b"const ") {
                            reassigned.insert(name.as_str());
                        }
                    }
                    // Also check for ++, --, +=, -=, etc.
                    if k + 1 < bytes.len() && ((bytes[k] == b'+' && bytes[k+1] == b'+') || (bytes[k] == b'-' && bytes[k+1] == b'-') || (bytes[k] == b'+' && bytes[k+1] == b'=') || (bytes[k] == b'-' && bytes[k+1] == b'=')) {
                        reassigned.insert(name.as_str());
                    }
                    // Check prefix ++ / --
                    if i >= 2 && ((bytes[i-1] == b'+' && bytes[i-2] == b'+') || (bytes[i-1] == b'-' && bytes[i-2] == b'-')) {
                        reassigned.insert(name.as_str());
                    }
                }
            }
            i += 1;
        }
    }

    // Third pass: replace `let` with `const` for non-reassigned names
    let mut result = src.to_string();
    // Process in reverse order so byte offsets remain valid
    for (offset, name) in let_decls.iter().rev() {
        if !reassigned.contains(name.as_str()) {
            // Replace "let" with "const" at this offset (3 bytes → 5 bytes)
            result.replace_range(*offset..*offset + 3, "const");
        }
    }
    result
}

fn normalize_arrow_params(src: &str) -> String {
    let s = src.trim_start();
    // Find the `=>` token (ignoring nested strings/parens).
    // If source starts with `(`, it's already parenthesized.
    if s.starts_with('(') || s.starts_with("async ") || s.starts_with("async(") {
        return src.to_string();
    }
    // Check if the source is `IDENT => ...` (single unparenthesized param).
    let bytes = s.as_bytes();
    let mut i = 0;
    if i < bytes.len() && (bytes[i].is_ascii_alphabetic() || bytes[i] == b'_' || bytes[i] == b'$') {
        let start = i;
        while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' || bytes[i] == b'$') {
            i += 1;
        }
        let param = &s[start..i];
        // Skip whitespace.
        while i < bytes.len() && bytes[i] == b' ' { i += 1; }
        // Check for `=>`.
        if i + 1 < bytes.len() && bytes[i] == b'=' && bytes[i + 1] == b'>' {
            let rest = &s[i..];
            // Reconstruct with parens.
            let leading_spaces = src.len() - src.trim_start().len();
            let indent = &src[..leading_spaces];
            return format!("{indent}({param}) {rest}");
        }
    }
    src.to_string()
}

/// If `expr` is a no-arg arrow function body of the form `() => { return EXPR; }`,
/// extract and return `EXPR`. Otherwise return `None`.
///
/// This handles the IIFE pattern where HIR emits a FunctionExpression that is
/// immediately called with no args: `(() => { return EXPR; })()` → `EXPR`.
fn extract_iife_return_expr(expr: &str) -> Option<String> {
    let s = expr.trim();
    // Must start with `() =>`
    let s = s.strip_prefix("()")?;
    let s = s.trim_start();
    let s = s.strip_prefix("=>")?;
    let s = s.trim_start();

    // Case 1: Block body `{ return EXPR; }`
    if let Some(body) = s.strip_prefix('{') {
        let body = body.trim_start();
        let body = body.strip_prefix("return")?;
        if body.is_empty() || !body.starts_with(|c: char| c.is_whitespace()) {
            return None;
        }
        let body = body.trim_start();
        let body = body.trim_end();
        let body = body.strip_suffix('}')?;
        let body = body.trim_end();
        let body = body.strip_suffix(';').unwrap_or(body);
        let body = body.trim_end();
        if body.is_empty() { return None; }
        return Some(body.to_string());
    }

    // Case 2: Expression body `() => EXPR` (no braces)
    if !s.is_empty() {
        return Some(s.to_string());
    }

    None
}

/// Re-indent a potentially multi-line expression so that it looks correct when
/// embedded as a statement at `body_pad` indentation.
///
/// The first line is NOT re-indented (the caller already writes `{body_pad}{first_line}`).
/// Subsequent lines are adjusted so the last line (closing brace) sits at `body_pad`.
///
/// Algorithm:
///   1. Determine the base indent of the expression by looking at the LAST non-empty line.
///      (For arrow functions, this is the closing `}` or `};`.)
///   2. Compute delta = target_base_len - original_base_len.
///   3. For each line after the first: add `delta` spaces to its indent.
///
/// If the expression is single-line, it is returned unchanged.
/// Returns true if `name` is a global function that returns a primitive value
/// (string, number, boolean) and is therefore safe to hoist as a `const tN = name(arg)`
/// before a reactive scope block.
fn is_primitive_returning_global(name: &str) -> bool {
    matches!(name,
        "String" | "Number" | "Boolean" | "parseInt" | "parseFloat"
        | "isNaN" | "isFinite" | "encodeURIComponent" | "decodeURIComponent"
        | "encodeURI" | "decodeURI" | "typeof"
    )
}

/// Resolve an identifier through a chain of LoadLocal/LoadContext instructions,
/// returning the ultimate source identifier. Used to match call arguments back
/// to named variables (scope deps).
fn resolve_through_loads(
    id: crate::hir::hir::IdentifierId,
    instr_map: &HashMap<u32, Instruction>,
) -> crate::hir::hir::IdentifierId {
    let mut current = id;
    loop {
        match instr_map.get(&current.0) {
            Some(instr) => match &instr.value {
                InstructionValue::LoadLocal { place, .. }
                | InstructionValue::LoadContext { place, .. } => {
                    current = place.identifier;
                }
                _ => return current,
            },
            None => return current,
        }
    }
}

fn reindent_multiline(expr: &str, body_pad: &str) -> String {
    let lines: Vec<&str> = expr.lines().collect();
    if lines.len() <= 1 {
        return expr.to_string();
    }

    // Determine original base indent from the last non-empty line.
    let original_base = lines.iter().rev()
        .find(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start().len())
        .unwrap_or(0);

    // Only reindent if the expression has a meaningful base indent (> 0).
    // If the expression's last line is at column 0 (e.g., top-level JSX or
    // already-normalized function), do not adjust — we don't know the original
    // indentation context and might add too many spaces.
    if original_base == 0 {
        return expr.to_string();
    }

    let target_base = body_pad.len();
    let delta: i64 = target_base as i64 - original_base as i64;

    let mut result = String::new();
    for (i, line) in lines.iter().enumerate() {
        if i == 0 {
            result.push_str(line);
        } else {
            // Adjust indent of this line by delta.
            let current_indent = line.len() - line.trim_start().len();
            let new_indent = (current_indent as i64 + delta).max(0) as usize;
            result.push('\n');
            for _ in 0..new_indent {
                result.push(' ');
            }
            result.push_str(line.trim_start());
        }
    }
    result
}
