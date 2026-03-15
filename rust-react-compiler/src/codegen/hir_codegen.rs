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
    JsxAttribute, JsxTag, LogicalOperator, LValuePattern, NonLocalBinding, ObjectExpressionProperty,
    ObjectProperty, ObjectPropertyKey, ObjectPatternProperty, Param, Pattern, Place, PrimitiveValue,
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
    let (body, outlines) = codegen_hir_function_parts(hir, env);
    let mut out = body;
    for outline in outlines {
        out.push('\n');
        out.push_str(&outline);
        out.push('\n');
    }
    out
}

/// Like `codegen_hir_function`, but returns the component body and outlined helpers separately.
/// Callers that want to append outlined helpers at the module end (after all other declarations)
/// should use this instead of `codegen_hir_function`.
pub fn codegen_hir_function_parts(hir: &HIRFunction, env: &Environment) -> (String, Vec<String>) {
    let mut gen = Codegen::new(hir, env);
    let out = gen.emit();
    // Collect outlined function bodies separately.
    let outlines: Vec<String> = env.outlined_functions.iter().map(|(_name, decl)| {
        let normalized = normalize_fn_body_text(decl);
        let normalized = normalize_jsx_self_closing(&normalized);
        reindent_multiline(&normalized, "")
    }).collect();
    (out, outlines)
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

/// Replace all word-boundary occurrences of `alias.` with `ns_name.` in source text.
/// Used to resolve namespace aliases in function body source (JSX tags, member access).
/// e.g. `<localVar.Text>` → `<SharedRuntime.Text>` when alias="localVar", ns_name="SharedRuntime".
fn rename_namespace_in_src(src: &str, alias: &str, ns_name: &str) -> String {
    if alias.is_empty() || alias == ns_name { return src.to_string(); }
    let pattern = format!("{alias}.");
    let mut result = src.to_string();
    let mut start = 0;
    while let Some(rel_pos) = result[start..].find(&pattern) {
        let pos = start + rel_pos;
        // Word-boundary check: char before `alias` must not be an identifier char
        let before_ok = pos == 0 || {
            let c = result[..pos].chars().next_back().unwrap_or('\0');
            !(c.is_alphanumeric() || c == '_' || c == '$')
        };
        if before_ok {
            let replacement = format!("{ns_name}.");
            result = format!("{}{}{}", &result[..pos], replacement, &result[pos + pattern.len()..]);
            start = pos + replacement.len();
        } else {
            start = pos + 1;
        }
    }
    result
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
    /// True if this output comes from a Reassign StoreLocal (variable declared before scope).
    /// Used to order cache slots: non-reassign declarations first, sentinel second, reassigns last.
    is_reassign: bool,
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
    /// If Some(idx), the scope's last instruction is a Destructure that should be
    /// emitted post-scope (after the if/else block) using the scope output variable
    /// as its value. The value is idx into the scope's instruction slice.
    post_scope_destructure_idx: Option<usize>,
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
    /// Number of body temps promoted by build_promoted_temp_names.
    /// Catch variable renaming starts at param_name_offset + promoted_temp_count.
    promoted_temp_count: usize,
    /// Instruction map: instr lvalue identifier id → Instruction
    instr_map: HashMap<u32, Instruction>,
    /// Use count: how many times each identifier is used as an operand.
    use_count: HashMap<u32, u32>,
    /// Sum of use counts across all SSA versions of the same declared variable.
    /// Key is DeclarationId.0. Used for SSA-version-aware usage checks in destructuring.
    decl_id_use_count: HashMap<u32, u32>,
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
    /// Maps identifier id → renamed temp name for promoted $t/$T vars.
    /// Pre-populated before emission so destructuring bindings and other
    /// $t-prefixed identifiers get sequential t0, t1, ... names (matching
    /// the reference compiler's rename_variables pass behavior).
    promoted_temp_names: HashMap<u32, String>,
    /// Maps ScopeId → set of variable names declared (via DeclareLocal) before
    /// the scope's start in program order. Used by emit_scope_block_inner to
    /// avoid re-declaring variables that already exist in outer blocks.
    declared_names_before_scope: HashMap<ScopeId, std::collections::HashSet<String>>,
    /// Set of DeclarationIds that are targets of reassignment or update expressions.
    /// Used to emit `let` instead of `const` for destructuring patterns whose
    /// bound variables are later mutated (e.g., `let { c } = t0` when `c++` follows).
    reassigned_decl_ids: std::collections::HashSet<DeclarationId>,
    /// Set of fallthrough BlockIds that belong to Label terminals (as opposed to switches).
    /// Used to distinguish natural exits (no break needed) from switch exits (break needed).
    label_fallthrough_blocks: std::collections::HashSet<BlockId>,
    /// Set of ReactiveLabel block IDs that correspond to early-return labeled blocks.
    /// These get emitted as `bb0: { ... }` (with braces) rather than `bb0: switch { ... }`.
    early_return_label_blocks: std::collections::HashSet<BlockId>,
    /// When inside an early-return scope body, Some((sentinel_var_name, label_name)).
    /// Used by emit_scope_body_cfg_walk to transform `return val` → `sentinel = val; break label`.
    active_early_return: Option<(String, String)>,
    /// Blocks that were processed as early-return branch bodies inside a scope's cfg walk.
    /// These blocks' Return terminals were already transformed; emit_cfg_region must skip them.
    early_return_handled_blocks: std::collections::HashSet<BlockId>,
    /// Set of InstructionIds for DeclareLocal instructions that have been hoisted to before
    /// the scope block. When emit_scope_body_cfg_walk encounters these, it skips them.
    hoisted_declare_instr_ids: std::collections::HashSet<InstructionId>,
    /// Set of BlockIds for label body blocks that were already emitted inside a scope body.
    /// When emit_cfg_region hits a Label terminal, it checks this set to avoid double-emission.
    scope_emitted_label_bodies: std::collections::HashSet<BlockId>,
    /// Maps local variable names that are aliases for namespace imports to the namespace name.
    /// e.g. `const MyLocal = SharedRuntime` where SharedRuntime is `import * as SharedRuntime`
    /// → `"MyLocal" → "SharedRuntime"`. Used to resolve JSX member-expression tags.
    namespace_alias: HashMap<String, String>,
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
    // Exclude Destructure instructions that source from a parameter (mutable_range.start == 0).
    // Parameter destructures should always be emitted outside scope blocks so that their
    // extracted values are available for use as scope dependencies in the scope check condition.
    if let InstructionValue::Destructure { value, .. } = &instr.value {
        if env.get_identifier(value.identifier)
            .map(|i| i.mutable_range.start.0 == 0)
            .unwrap_or(false)
        {
            return false;
        }
    }
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
            // If this scope has an early-return sentinel, reserve one extra slot for it.
            let extra = if scope.early_return_value.is_some() { 1 } else { 0 };
            if std::env::var("RC_DEBUG").is_ok() {
                eprintln!("[count_scope_outputs] scope {:?} n_escaping={} extra={} early_return={:?}",
                    sid.0, n_escaping, extra, scope.early_return_value);
            }
            result.insert(sid, n_escaping + extra);
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
                    // (c) Instructions within scope range that consume scope-owned operands
                    // are themselves scope-internal. This handles array/call/JSX constructions.
                    if !scope_instr_lvalue_ids.contains(&instr.lvalue.identifier.0) {
                        let in_range = instr.id.0 >= scope.range.start.0
                            && instr.id.0 < scope.range.end.0;
                        if in_range {
                            let ops = each_instruction_value_operand(&instr.value);
                            let any_scope_operand = ops.iter()
                                .any(|op| scope_instr_lvalue_ids.contains(&op.identifier.0));
                            if any_scope_operand {
                                scope_instr_lvalue_ids.insert(instr.lvalue.identifier.0);
                            }
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
                n_ssa_outputs += 1;
            }
        }
        let base = n_ssa_outputs.max(1);
        // If this scope has an early-return sentinel, reserve one extra slot for it.
        let extra = if scope.early_return_value.is_some() { 1 } else { 0 };
        if std::env::var("RC_DEBUG").is_ok() {
            eprintln!("[count_scope_outputs] scope {:?} n_escaping={} n_ssa={} base={} extra={} early_return={:?}",
                sid.0, n_escaping, n_ssa_outputs, base, extra, scope.early_return_value);
        }
        result.insert(sid, base + extra);
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
            // Count phi operands — they are uses of instructions that feed into join blocks.
            for phi in &block.phis {
                for (_, op) in &phi.operands {
                    *use_count.entry(op.identifier.0).or_insert(0) += 1;
                }
            }
        }

        // Build declaration_id -> total use count for SSA-version-aware usage checks.
        // When SSA creates phi nodes for variables captured in closures, the phi ids
        // may have use_count > 0 but the Destructure-created ids may have use_count == 0.
        // Using declaration_id sums use counts across all SSA versions of the same variable.
        let mut decl_id_use_count: HashMap<u32, u32> = HashMap::new();
        for (&raw_id, &cnt) in &use_count {
            if let Some(ident) = env.get_identifier(crate::hir::hir::IdentifierId(raw_id)) {
                *decl_id_use_count.entry(ident.declaration_id.0).or_insert(0) += cnt;
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
            promoted_temp_count: 0, // will be set after build_promoted_temp_names
            instr_map,
            use_count,
            decl_id_use_count,
            terminal_replacement: HashMap::new(),
            ssa_value_to_name,
            instr_to_block,
            within_loop_scopes: std::collections::HashSet::new(),
            inline_js_referenced_ids,
            name_overrides,
            switch_labels: HashMap::new(),
            switch_fallthrough_labels: HashMap::new(),
            scope_output_names: HashMap::new(),
            promoted_temp_names: HashMap::new(),
            declared_names_before_scope: HashMap::new(),
            reassigned_decl_ids,
            label_fallthrough_blocks: std::collections::HashSet::new(),
            early_return_label_blocks: std::collections::HashSet::new(),
            active_early_return: None,
            early_return_handled_blocks: std::collections::HashSet::new(),
            hoisted_declare_instr_ids: std::collections::HashSet::new(),
            scope_emitted_label_bodies: std::collections::HashSet::new(),
            namespace_alias: HashMap::new(),
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

        // Register early-return labeled blocks from env.scopes.
        // These are created by propagate_early_returns and need labels for break/label emission.
        for (_, scope) in &self.env.scopes {
            if let Some(label_id) = scope.early_return_label_id {
                let label_str = format!("bb{label_counter}");
                self.switch_fallthrough_labels.insert(label_id, label_str);
                self.early_return_label_blocks.insert(label_id);
                label_counter += 1;
            }
        }

        // Collect instructions in block order.
        let ordered = self.collect_instructions_in_order();

        // Populate namespace_alias: detect `const localVar = NS` patterns where NS is a
        // namespace import (`import * as NS from ...`).
        // e.g. `const localVar = SharedRuntime; <localVar.Text>` → register "localVar" → "SharedRuntime".
        // The environment's `namespace_import_names` contains all names from `import * as NS`.
        if !self.env.namespace_import_names.is_empty() {
            // Build a map: LoadGlobal result id → namespace name (only for namespace imports)
            let mut load_ns_id: std::collections::HashMap<u32, String> = std::collections::HashMap::new();
            for instr in &ordered {
                if let InstructionValue::LoadGlobal { binding, .. } = &instr.value {
                    let name = match binding {
                        crate::hir::hir::NonLocalBinding::Global { name } => name.clone(),
                        crate::hir::hir::NonLocalBinding::ImportNamespace { name, .. } => name.clone(),
                        crate::hir::hir::NonLocalBinding::ImportSpecifier { name, .. } => name.clone(),
                        crate::hir::hir::NonLocalBinding::ImportDefault { name, .. } => name.clone(),
                        crate::hir::hir::NonLocalBinding::ModuleLocal { name } => name.clone(),
                    };
                    if self.env.namespace_import_names.contains(&name) {
                        load_ns_id.insert(instr.lvalue.identifier.0, name);
                    }
                }
            }
            // Find StoreLocal where the value is a namespace import LoadGlobal
            for instr in &ordered {
                if let InstructionValue::StoreLocal { lvalue, value, .. } = &instr.value {
                    if let Some(ns_name) = load_ns_id.get(&value.identifier.0) {
                        if let Some(id_info) = self.env.identifiers.get(&lvalue.place.identifier) {
                            if let Some(var_name) = &id_info.name {
                                let local_name = var_name.value().to_string();
                                if local_name != *ns_name {
                                    // Only register if the local name differs from the namespace name
                                    self.namespace_alias.insert(local_name, ns_name.clone());
                                }
                            }
                        }
                    }
                }
            }
        }

        // Build inlined_exprs for transparent single-use temps.
        self.build_inline_map(&ordered);

        // Resolve phi results from logical expressions (&&, ||, ??) into
        // inlined JS expressions so they emit as `a && b` instead of `$tN`.
        self.resolve_logical_phis();

        // Build "should_inline" set: instructions that are fully inlined
        // and should NOT produce standalone statements.
        let inlined_ids = self.collect_inlined_ids(&ordered);

        // Determine which scope each instruction belongs to.
        // Must happen before build_promoted_temp_names so we can restrict scope_decl_ids
        // to only scopes that are actually emitted (pruned scopes should not block renaming).
        let instr_scope = self.assign_instructions_to_scopes(&ordered);

        // Rename $t/$T promoted temp identifiers to sequential t0, t1, ... names.
        // This must run before scope emission so the assigned names don't conflict.
        // Returns the number of temps assigned (scope_index starts at this offset).
        let promoted_temp_count = self.build_promoted_temp_names(&ordered, &inlined_ids, &instr_scope);
        self.promoted_temp_count = promoted_temp_count;

        // Rebuild inlined_exprs now that promoted_temp_names is populated.
        // The first build_inline_map call above used fallback names (e.g. $t18);
        // we need to redo it so inlined expression strings reference t0/t1/... names.
        self.inlined_exprs.clear();
        self.build_inline_map(&ordered);
        self.resolve_logical_phis();
        // Substitute any stale $tN references in build_inline_map entries.
        // build_inline_map ran before resolve_logical_phis, so BinaryExpression
        // inlined values like "x + $t471" may reference ternary phi results that
        // are now in inlined_exprs. Substitute them using the ID-keyed map.
        // Note: "$tN" in expression strings uses N = IdentifierId.0 (the raw u32),
        // which is the same as the inlined_exprs map key.
        {
            let keys: Vec<u32> = self.inlined_exprs.keys().copied().collect();
            for _ in 0..8 {
                let mut changed = false;
                for &k in &keys {
                    let expr = match self.inlined_exprs.get(&k) {
                        Some(e) if e.contains("$t") => e.clone(),
                        _ => continue,
                    };
                    let new_expr = Self::substitute_temp_refs_in_str(&expr, &self.inlined_exprs);
                    if new_expr != expr {
                        self.inlined_exprs.insert(k, new_expr);
                        changed = true;
                    }
                }
                if !changed { break; }
            }
        }

        let mut out = String::new();

        let fn_name = self.hir.id.as_deref().unwrap_or("anonymous");
        let async_kw = if self.hir.async_ { "async " } else { "" };
        let params = self.emit_params();

        // Only emit the runtime import when there are actual cache slots.
        // Choose a non-conflicting alias for the `c` import: start with `_c`,
        // then `_c2`, `_c3`, ... if `_c` is already a name in the source.
        let cache_fn_name = if self.num_scopes > 0 {
            let source_names: std::collections::HashSet<String> = self.env.identifiers.values()
                .filter_map(|id| id.name.as_ref().map(|n| n.value().to_string()))
                .collect();
            let mut alias = "_c".to_string();
            let mut suffix = 2u32;
            while source_names.contains(&alias) {
                alias = format!("_c{suffix}");
                suffix += 1;
            }
            let _ = writeln!(out, "import {{ c as {alias} }} from \"react/compiler-runtime\";");
            alias
        } else {
            "_c".to_string()
        };
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
            let _ = writeln!(out, "  const $ = {}({});", cache_fn_name, self.num_scopes);
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

        // Build declared_names_before_scope: for each scope, collect variable names
        // declared (via DeclareLocal/DeclareContext) in non-scope instructions that
        // appear before the scope's first instruction in program order.
        // This prevents double-declaration when a scope output has the same name as
        // an outer `let` declaration.
        {
            let mut running_declared: std::collections::HashSet<String> = std::collections::HashSet::new();
            let mut scope_first_seen: std::collections::HashSet<ScopeId> = std::collections::HashSet::new();
            let mut scope_declared_map: HashMap<ScopeId, std::collections::HashSet<String>> = HashMap::new();
            for instr in &ordered {
                if let Some(&sid) = instr_scope.get(&instr.id) {
                    if !scope_first_seen.contains(&sid) {
                        scope_first_seen.insert(sid);
                        scope_declared_map.insert(sid, running_declared.clone());
                    }
                } else {
                    let declared_id = match &instr.value {
                        InstructionValue::DeclareLocal { lvalue, .. } => {
                            Some(lvalue.place.identifier)
                        }
                        InstructionValue::DeclareContext { lvalue, .. } => {
                            Some(lvalue.place.identifier)
                        }
                        _ => None,
                    };
                    if let Some(id) = declared_id {
                        if let Some(name) = self.env.get_identifier(id)
                            .and_then(|i| i.name.as_ref())
                            .map(|n| n.value().to_string())
                        {
                            running_declared.insert(name);
                        }
                    }
                }
            }
            self.declared_names_before_scope = scope_declared_map;
        }

        // scope_index counts scopes in emission order for temp naming.
        // Starts at promoted_temp_count so scope output names (tN) don't conflict
        // with promoted temp names (t0, t1, ... assigned above).
        let mut scope_index: usize = promoted_temp_count;
        let mut emitted_scopes: std::collections::HashSet<ScopeId> = std::collections::HashSet::new();
        let mut visited: std::collections::HashSet<BlockId> = std::collections::HashSet::new();
        let mut inlined_ids_mut = inlined_ids.clone();

        // Use tree-based emission if enabled via env var (for testing/development).
        // Use tree-based codegen when RC_TREE_CODEGEN is set and reactive_block is available.
        // Flat CFG-based codegen is the default until tree codegen reaches parity.
        let use_tree = std::env::var("RC_TREE_CODEGEN").is_ok() && self.hir.reactive_block.is_some();
        if use_tree {
            if let Some(reactive_block) = self.hir.reactive_block.clone() {
                let mut declared_names = std::collections::HashSet::new();
                self.codegen_tree_block(
                    &reactive_block, &[], 1, &mut out,
                    &scope_instrs_map, &inlined_ids_mut,
                    &mut scope_index, &mut declared_names,
                );
            }
        } else {
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
        }

        if self.hir.is_arrow {
            let _ = writeln!(out, "}};");
        } else {
            let _ = writeln!(out, "}}");
        }
        // Post-process: fix _c(N) to match actual max $[M] usage.
        // Our pre-computed num_scopes may overcount output slots.
        if self.num_scopes > 0 {
            let mut max_slot: i32 = -1;
            let bytes = out.as_bytes();
            let mut i = 0;
            while i + 2 < bytes.len() {
                if bytes[i] == b'$' && bytes[i + 1] == b'[' {
                    let start = i + 2;
                    let mut j = start;
                    while j < bytes.len() && bytes[j].is_ascii_digit() {
                        j += 1;
                    }
                    if j > start && j < bytes.len() && bytes[j] == b']' {
                        if let Ok(n) = out[start..j].parse::<i32>() {
                            if n > max_slot { max_slot = n; }
                        }
                    }
                    i = j;
                } else {
                    i += 1;
                }
            }
            let actual_slots = (max_slot + 1) as usize;
            if actual_slots != self.num_scopes && actual_slots > 0 {
                let old = format!("{}({});", cache_fn_name, self.num_scopes);
                let new = format!("{}({});", cache_fn_name, actual_slots);
                out = out.replacen(&old, &new, 1);
            }
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
                        if let InstructionValue::GetIterator { collection, .. } = &instr.value {
                            inlined_ids.insert(instr.lvalue.identifier.0);
                            // If the collection comes from a MethodCall, CallExpression,
                            // or a literal (ArrayExpression/ObjectExpression), inline it so
                            // the expression appears directly in the for-of header.
                            // This prevents array/object literals that merged into a sentinel
                            // scope from being emitted inside the scope's if-block and then
                            // referenced outside (which would be a ReferenceError).
                            if let Some(coll_instr) = self.instr_map.get(&collection.identifier.0) {
                                match &coll_instr.value {
                                    InstructionValue::MethodCall { .. }
                                    | InstructionValue::CallExpression { .. }
                                    | InstructionValue::ArrayExpression { .. }
                                    | InstructionValue::ObjectExpression { .. }
                                    | InstructionValue::PropertyLoad { .. }
                                    | InstructionValue::ComputedLoad { .. }
                                    | InstructionValue::LoadLocal { .. } => {
                                        inlined_ids.insert(collection.identifier.0);
                                    }
                                    _ => {}
                                }
                            }
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
                if std::env::var("RC_DEBUG_INSTR").is_ok() {
                    eprintln!("[DEBUG_INSTR] lv_id={} discriminant={:?} inlined={} in_scope={:?}",
                        lv_id, std::mem::discriminant(&instr.value),
                        inlined_ids.contains(&lv_id), instr_scope.get(&instr.id));
                }

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
                        let decl_names_before = self.declared_names_before_scope.get(&sid).cloned().unwrap_or_default();
                        self.emit_scope_block_inner(
                            &sid,
                            &scope_instr_refs,
                            indent,
                            scope_index,
                            out,
                            inlined_ids,
                            &decl_names_before,
                            None,
                        );
                        emitted_scopes.insert(sid);
                        // Mark all instructions absorbed into this scope's body as handled,
                        // so the CFG walker doesn't re-emit them later (e.g., outlined function
                        // declarations that are absorbed into the scope body via group_by_scope
                        // but have no instr_scope entry because they're early-excluded).
                        for si in &scope_instrs_list {
                            inlined_ids.insert(si.lvalue.identifier.0);
                        }
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
                    let body_pad = indent + 1;

                    // Buffer if-body. Detect memoization-emptied if-blocks
                    // (block has instructions but they all got emitted as scope blocks
                    // outside the if) vs genuinely empty ones (no instructions in HIR).
                    let consequent_has_instrs = self.cfg_region_has_instructions(*consequent, *fallthrough);
                    let mut if_buf = String::new();
                    let mut vis2 = visited.clone();
                    // If the consequent block was handled as an early-return branch inside a scope's
                    // cfg walk, pre-mark it visited so emit_cfg_region doesn't re-emit its Return.
                    for &bid in &self.early_return_handled_blocks.clone() {
                        vis2.insert(bid);
                    }
                    self.emit_cfg_region(
                        *consequent, Some(*fallthrough), body_pad, &mut if_buf,
                        &mut vis2, emitted_scopes, scope_index, instr_scope, inlined_ids, scope_instrs,
                    );
                    // If the consequent region had instructions in the HIR but the
                    // emitted if-body is empty, those instructions were all memoization
                    // scope instructions (already emitted before this terminal).
                    let if_memoization_emptied = if_buf.trim().is_empty() && consequent_has_instrs;

                    let emit_else = *alternate != *fallthrough;
                    let mut else_buf = String::new();
                    if emit_else {
                        // Emit else body to temp buffer; skip if empty.
                        let mut vis3 = visited.clone();
                        // Pre-mark early-return handled blocks as visited so
                        // emit_cfg_region doesn't re-emit Return terminals that were
                        // already transformed inside an early-return scope body.
                        for &bid in &self.early_return_handled_blocks.clone() {
                            vis3.insert(bid);
                        }
                        self.emit_cfg_region(
                            *alternate, Some(*fallthrough), body_pad, &mut else_buf,
                            &mut vis3, emitted_scopes, scope_index, instr_scope, inlined_ids, scope_instrs,
                        );
                    }

                    let if_non_empty = !if_buf.trim().is_empty();
                    let else_non_empty = !else_buf.trim().is_empty();

                    // Suppress the entire if statement only when the if-body was
                    // emptied by memoization AND the else-body is also empty.
                    // Genuinely empty if-bodies (no instructions in the CFG block)
                    // are preserved (they exist in the source).
                    if if_non_empty || else_non_empty || !if_memoization_emptied {
                        let _ = writeln!(out, "{pad}if ({test_expr}) {{");
                        out.push_str(&if_buf);
                        if else_non_empty {
                            let _ = writeln!(out, "{pad}}} else {{");
                            out.push_str(&else_buf);
                        }
                        let _ = writeln!(out, "{pad}}}");
                    }

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
                    // Use do_while_test_expr to handle logical-chain conditions (&&/||).
                    // The While.test block may itself be a logical-op branch; follow the
                    // chain to the final Branch with the resolved phi result.
                    let test_expr = if self.hir.body.blocks.get(&test_bid)
                        .map(|b| matches!(&b.terminal, Terminal::Branch { .. }))
                        .unwrap_or(false)
                    {
                        self.do_while_test_expr(test_bid)
                    } else {
                        "true".to_string()
                    };
                    // Mark test block and entire logical chain as visited so the
                    // recursive body walk doesn't re-enter them. This mirrors the
                    // for-loop handling.
                    {
                        let mut tb = test_bid;
                        for _ in 0..32 {
                            visited.insert(tb);
                            let next = self.hir.body.blocks.get(&tb).and_then(|b| {
                                if let Terminal::Branch { fallthrough, logical_op: Some(_), .. } = &b.terminal {
                                    Some(*fallthrough)
                                } else {
                                    None
                                }
                            });
                            match next { Some(n) => tb = n, None => break }
                        }
                    }
                    let _ = writeln!(out, "{pad}while ({test_expr}) {{");
                    let body_pad = indent + 1;
                    let mut vis2 = visited.clone();
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

                    // Detect unconditional break: `do { [stmts...]; break; } while (cond)`.
                    // If the loop body's first block breaks directly to fallthrough,
                    // the loop condition never executes — emit the body without any
                    // loop wrapper (regardless of whether a scope is deferred).
                    let loop_body_always_breaks = self.hir.body.blocks.get(&loop_bid)
                        .map(|b| matches!(&b.terminal,
                            Terminal::Goto { block: dest, variant: GotoVariant::Break, .. }
                            if *dest == fall_bid
                        ))
                        .unwrap_or(false);

                    if loop_body_always_breaks {
                        // Emit body instructions without the do-while wrapper.
                        let body_pad = indent;
                        let mut vis2 = visited.clone();
                        vis2.insert(test_bid);
                        vis2.insert(fall_bid);
                        self.emit_cfg_region(
                            loop_bid, Some(fall_bid), body_pad, out,
                            &mut vis2, emitted_scopes, scope_index, instr_scope, inlined_ids, scope_instrs,
                        );
                        if Some(fall_bid) == stop_at { return; }
                        current = fall_bid;
                        continue;
                    }

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
                    let test_expr = self.do_while_test_expr(test_bid);
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

                    // Use do_while_test_expr to handle logical-chain conditions (&&/||).
                    // The For.test block may itself be a logical-op branch; follow the
                    // chain to the final Branch with the resolved phi result.
                    let test_expr = if self.hir.body.blocks.get(&test_bid)
                        .map(|b| matches!(&b.terminal, Terminal::Branch { .. }))
                        .unwrap_or(false)
                    {
                        self.do_while_test_expr(test_bid)
                    } else {
                        "true".to_string()
                    };

                    // Reconstruct update expression by walking the entire update sub-CFG
                    // (from update_bid to just before test_bid). Complex updates (ternary
                    // conditionals) span multiple blocks; collect all non-inlined instructions
                    // and join as comma-separated expressions.
                    let update_expr = if let Some(ubid) = update_bid {
                        let exprs = self.collect_for_update_exprs(ubid, test_bid);
                        exprs.join(", ")
                    } else {
                        String::new()
                    };

                    let _ = writeln!(out, "{pad}for ({init_expr}; {test_expr}; {update_expr}) {{");
                    let body_pad = indent + 1;
                    let mut vis2 = visited.clone();
                    // Mark entire test sub-CFG (logical chain) as visited.
                    {
                        let mut tb = test_bid;
                        for _ in 0..32 {
                            vis2.insert(tb);
                            let next = self.hir.body.blocks.get(&tb).and_then(|b| {
                                if let Terminal::Branch { fallthrough, logical_op: Some(_), .. } = &b.terminal {
                                    Some(*fallthrough)
                                } else {
                                    None
                                }
                            });
                            match next { Some(n) => tb = n, None => break }
                        }
                    }
                    // Mark entire update sub-CFG as visited.
                    if let Some(ubid) = update_bid {
                        use std::collections::VecDeque;
                        let mut q: VecDeque<crate::hir::hir::BlockId> = VecDeque::new();
                        q.push_back(ubid);
                        while let Some(bid) = q.pop_front() {
                            if vis2.contains(&bid) || bid == test_bid { continue; }
                            vis2.insert(bid);
                            if let Some(b) = self.hir.body.blocks.get(&bid) {
                                for succ in b.terminal.successors() {
                                    if succ != test_bid && !vis2.contains(&succ) {
                                        q.push_back(succ);
                                    }
                                }
                            }
                        }
                    }
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
                                InstructionValue::Primitive { value, .. } => {
                                    local_exprs.insert(instr.lvalue.identifier.0, primitive_expr(value));
                                }
                                InstructionValue::ArrayExpression { elements, .. } => {
                                    // Inline array literal (e.g. `[1, 2]`) so for-of can use
                                    // the literal directly instead of a scope-internal temp.
                                    let elems: Vec<String> = elements.iter().map(|e| match e {
                                        ArrayElement::Place(p) => local_exprs.get(&p.identifier.0)
                                            .cloned()
                                            .unwrap_or_else(|| self.expr(p)),
                                        ArrayElement::Spread(s) => format!("...{}", local_exprs.get(&s.place.identifier.0)
                                            .cloned()
                                            .unwrap_or_else(|| self.expr(&s.place))),
                                        ArrayElement::Hole => String::new(),
                                    }).collect();
                                    local_exprs.insert(instr.lvalue.identifier.0, format!("[{}]", elems.join(", ")));
                                }
                                InstructionValue::LoadLocal { place, .. } => {
                                    // Prefer a previously-resolved local_exprs entry (handles the
                                    // case where the loaded variable itself resolves to a literal).
                                    let name = local_exprs.get(&place.identifier.0)
                                        .cloned()
                                        .unwrap_or_else(|| self.ident_name(place.identifier));
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
                                InstructionValue::MethodCall { receiver, property, args, .. } => {
                                    let recv = local_exprs.get(&receiver.identifier.0)
                                        .cloned()
                                        .unwrap_or_else(|| self.expr(receiver));
                                    let method_suffix = self.method_suffix_from_place(property);
                                    let args_str = args.iter().map(|a| match a {
                                        crate::hir::hir::CallArg::Place(p) => local_exprs.get(&p.identifier.0).cloned().unwrap_or_else(|| self.expr(p)),
                                        crate::hir::hir::CallArg::Spread(s) => format!("...{}", local_exprs.get(&s.place.identifier.0).cloned().unwrap_or_else(|| self.expr(&s.place))),
                                    }).collect::<Vec<_>>().join(", ");
                                    local_exprs.insert(instr.lvalue.identifier.0, format!("{recv}{method_suffix}({args_str})"));
                                }
                                InstructionValue::CallExpression { callee, args, .. } => {
                                    let callee_str = local_exprs.get(&callee.identifier.0)
                                        .cloned()
                                        .unwrap_or_else(|| self.expr(callee));
                                    let args_str = args.iter().map(|a| match a {
                                        crate::hir::hir::CallArg::Place(p) => local_exprs.get(&p.identifier.0).cloned().unwrap_or_else(|| self.expr(p)),
                                        crate::hir::hir::CallArg::Spread(s) => format!("...{}", local_exprs.get(&s.place.identifier.0).cloned().unwrap_or_else(|| self.expr(&s.place))),
                                    }).collect::<Vec<_>>().join(", ");
                                    local_exprs.insert(instr.lvalue.identifier.0, format!("{callee_str}({args_str})"));
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

                    // Check if loop body starts with a Destructure of the loop variable.
                    // If so, emit the destructuring pattern directly in the for-of header.
                    let for_of_pattern = self.try_inline_for_of_destructure(
                        loop_bid, iter_next_id, &loop_var_name, inlined_ids,
                    );

                    visited.insert(test_bid);

                    let _ = writeln!(out, "{pad}for (const {for_of_pattern} of {iterable_expr}) {{");
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
                    let binding_name = handler_binding.as_ref().map(|p| {
                        let name = self.lvalue_name(p);
                        // If catch variable has a user name AND is unused (use_count==0),
                        // rename it to a temp name to match the TS compiler's rename_variables.
                        let use_cnt = *self.use_count.get(&p.identifier.0).unwrap_or(&0);
                        let is_user_named = self.env.get_identifier(p.identifier)
                            .and_then(|id| id.name.as_ref())
                            .map(|n| matches!(n, crate::hir::hir::IdentifierName::Named(_)))
                            .unwrap_or(false);
                        if use_cnt == 0 && is_user_named {
                            format!("t{}", self.param_name_offset + self.promoted_temp_count)
                        } else {
                            name
                        }
                    });
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
                        let _ = writeln!(out, "{pad}}} catch {{");
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
                    // Emit each case body into a temp buffer for collapsing.
                    // Consecutive cases with identical bodies get collapsed into:
                    //   case A:
                    //   case B: { shared_body }
                    // Empty bodies collapse into:
                    //   case A:
                    //   case B:
                    //   default:
                    // Collect case labels, body blocks (for grouping), and emitted body strings.
                    // We group consecutive cases that point to the SAME block (natural fallthrough
                    // in the CFG means the case body is just the next case's entry block).
                    // This mirrors how Babel collapses switch cases with empty consequents.
                    let mut case_labels: Vec<String> = Vec::new();
                    let mut case_block_ids: Vec<crate::hir::hir::BlockId> = Vec::new();
                    let mut case_bodies: Vec<String> = Vec::new();
                    // Track which blocks we've already emitted a body for (to avoid double-emit
                    // when multiple cases share the same block).
                    let mut emitted_case_blocks: std::collections::HashSet<crate::hir::hir::BlockId> = std::collections::HashSet::new();
                    for case in cases {
                        let label = if let Some(t) = &case.test {
                            format!("case {}:", self.expr(t))
                        } else {
                            "default:".to_string()
                        };
                        // Only emit body for the first case with this block; subsequent cases
                        // pointing to the same block get an empty body (they are fallthrough labels).
                        let body_buf = if emitted_case_blocks.contains(&case.block) {
                            String::new()
                        } else {
                            emitted_case_blocks.insert(case.block);
                            let mut buf = String::new();
                            let mut vis2 = visited.clone();
                            self.emit_cfg_region(
                                case.block, Some(fall_bid), body_pad + 1, &mut buf,
                                &mut vis2, emitted_scopes, scope_index, instr_scope, inlined_ids, scope_instrs,
                            );
                            buf
                        };
                        case_labels.push(label);
                        case_block_ids.push(case.block);
                        case_bodies.push(body_buf);
                    }
                    // Group consecutive cases with the same block for collapsing.
                    // Case[i] is a fallthrough label if it shares its block with a later case.
                    let n_cases = case_labels.len();
                    let mut i = 0;
                    while i < n_cases {
                        // Find the run of cases with the same block starting at i.
                        let mut j = i + 1;
                        while j < n_cases && case_block_ids[j] == case_block_ids[i] {
                            j += 1;
                        }
                        // Cases i..j all share the same block.
                        // Emit i..j-1 as fallthrough labels (no body), then j-1 with body.
                        for k in i..j - 1 {
                            let _ = writeln!(out, "{case_pad}{}", case_labels[k]);
                        }
                        let last_label = &case_labels[j - 1];
                        // Use the body from the first case in the group (others have empty bodies
                        // because we only emit once per block).
                        let body = case_bodies[i..j].iter().find(|b| !b.is_empty()).map(|s| s.as_str()).unwrap_or("");
                        if body.trim().is_empty() {
                            // Empty body: emit label without braces.
                            let _ = writeln!(out, "{case_pad}{last_label}");
                        } else {
                            let _ = writeln!(out, "{case_pad}{last_label} {{");
                            out.push_str(body);
                            let _ = writeln!(out, "{case_pad}}}");
                        }
                        i = j;
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
                    // Skip if the label body was already emitted by emit_scope_body_cfg_walk.
                    if self.scope_emitted_label_bodies.contains(&body_bid) {
                        if Some(fall_bid) == stop_at {
                            return;
                        }
                        current = fall_bid;
                        continue;
                    }
                    let label_opt = self.switch_fallthrough_labels.get(&fall_bid).cloned();
                    if let Some(ref label) = label_opt {
                        // Emit body to a temp buffer first to check if it's a single compound statement.
                        // If so, emit `label: stmt` (no outer braces); otherwise `label: { ... }`.
                        let mut temp_body = String::new();
                        let body_pad = indent + 1;
                        let mut vis2 = visited.clone();
                        self.emit_cfg_region(
                            body_bid, Some(fall_bid), body_pad, &mut temp_body,
                            &mut vis2, emitted_scopes, scope_index, instr_scope, inlined_ids, scope_instrs,
                        );
                        // Check if body is a single compound statement at body_pad indent.
                        let inner_prefix = "  ".repeat(body_pad);
                        let outer_prefix = "  ".repeat(indent);
                        let top_level_stmts: Vec<&str> = temp_body.lines()
                            .filter(|l| {
                                if !l.starts_with(&inner_prefix) { return false; }
                                if l.starts_with(&format!("{}  ", inner_prefix)) { return false; }
                                let trimmed = l.trim();
                                if trimmed.is_empty() { return false; }
                                // Skip closing braces and continuation keywords (} else {, } catch, etc.)
                                if trimmed == "}" || trimmed == "};" { return false; }
                                if trimmed.starts_with("} else") { return false; }
                                if trimmed.starts_with("} catch") { return false; }
                                if trimmed.starts_with("} finally") { return false; }
                                if trimmed.starts_with("} while") { return false; }
                                true
                            })
                            .collect();
                        let is_single_compound = top_level_stmts.len() == 1 && {
                            let first = top_level_stmts[0].trim_start();
                            first.starts_with("if ") || first.starts_with("if(")
                                || first.starts_with("while ") || first.starts_with("while(")
                                || first.starts_with("switch ") || first.starts_with("switch(")
                                || first.starts_with("for ") || first.starts_with("for(")
                                || first.starts_with("bb")  // nested labeled block
                        };
                        if is_single_compound {
                            // Dedent body by 2 spaces and prepend label to first line.
                            let mut first_line = true;
                            for line in temp_body.lines() {
                                let dedented = if line.starts_with("  ") { &line[2..] } else { line };
                                if first_line && !dedented.trim().is_empty() {
                                    let _ = writeln!(out, "{outer_prefix}{label}: {}", dedented.trim_start());
                                    first_line = false;
                                } else if !first_line {
                                    let _ = writeln!(out, "{dedented}");
                                }
                            }
                        } else {
                            let _ = writeln!(out, "{pad}{label}: {{");
                            out.push_str(&temp_body);
                            let _ = writeln!(out, "{pad}}}");
                        }
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
        declared_names: &std::collections::HashSet<String>,
        tree_body: Option<(&[crate::hir::hir::ReactiveStatement], &std::collections::HashMap<ScopeId, Vec<Instruction>>)>,
    ) {
        let pad = "  ".repeat(indent);
        let body_pad = "  ".repeat(indent + 1);

        let dep_slot_list = self.dep_slots.get(scope_id).cloned().unwrap_or_default();
        let out_slot_list = self.output_slots.get(scope_id).cloned().unwrap_or_else(|| vec![0]);
        // For early-return scopes, the sentinel (early-return value) occupies the LAST output slot.
        // For non-early-return no-deps scopes, the memo-cache-sentinel check uses the FIRST output slot.
        let has_early_return = self.env.scopes.get(scope_id).and_then(|s| s.early_return_value).is_some();
        let sentinel_slot = if has_early_return {
            out_slot_list.last().copied().unwrap_or(0)
        } else {
            out_slot_list.first().copied().unwrap_or(0)
        };

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

        // Post-process: for outputs with is_named_var=true whose name is pre-declared
        // in an outer scope (via DeclareLocal before this scope in tree walk),
        // convert to temp+reassignment so we don't double-declare.
        for output in &mut analysis.outputs {
            if output.is_named_var {
                if let Some(ref name) = output.out_name {
                    if declared_names.contains(name.as_str()) {
                        // Find the StoreLocal that writes to this name to get skip_idx and cache_expr.
                        let found = instrs.iter().enumerate().find(|(_, instr)| {
                            if let InstructionValue::StoreLocal { lvalue, .. } = &instr.value {
                                self.env.get_identifier(lvalue.place.identifier)
                                    .and_then(|i| i.name.as_ref())
                                    .map(|n| n.value() == name.as_str())
                                    .unwrap_or(false)
                            } else {
                                false
                            }
                        });
                        if let Some((idx, instr)) = found {
                            if let InstructionValue::StoreLocal { value, .. } = &instr.value {
                                let value_expr = self.expr(value);
                                output.is_named_var = false;
                                output.out_kw = "";  // plain reassignment
                                output.skip_idx = Some(idx);
                                output.cache_expr = value_expr;
                            }
                        } else {
                            output.is_named_var = false;
                            output.out_kw = "";
                        }
                    }
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
                            // Also update inlined_exprs if this id's value is a complex expression.
                            // This ensures that post-scope StoreLocals using this value get `t` not stale body.
                            if let Some(old_val) = self.inlined_exprs.get(&value.identifier.0).cloned() {
                                if !old_val.starts_with("$t") {
                                    // old_val is a complex expression — also update other inlined_exprs
                                    // entries that have the same expression body (duplicate SSA ids).
                                    let cache_var = t.clone();
                                    let entries_to_update: Vec<u32> = self.inlined_exprs.iter()
                                        .filter(|(_, v)| v.as_str() == old_val.as_str())
                                        .map(|(k, _)| *k)
                                        .collect();
                                    for k in entries_to_update {
                                        self.inlined_exprs.insert(k, cache_var.clone());
                                    }
                                }
                            }
                        }
                        // Also propagate: if inlined_exprs[lv_id] has a complex body, update
                        // other inlined_exprs entries with the same body (e.g. duplicate FunctionExpression ids).
                        let lv_id = instr.lvalue.identifier.0;
                        if let Some(old_val) = self.inlined_exprs.get(&lv_id).cloned() {
                            if !old_val.starts_with("$t") {
                                let cache_var = t.clone();
                                let entries_to_update: Vec<u32> = self.inlined_exprs.iter()
                                    .filter(|(k, v)| **k != lv_id && v.as_str() == old_val.as_str())
                                    .map(|(k, _)| *k)
                                    .collect();
                                for k in entries_to_update {
                                    self.inlined_exprs.insert(k, cache_var.clone());
                                }
                            }
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

        // If this scope has an early-return sentinel, allocate a temp name for it.
        // The sentinel occupies the LAST output slot; regular outputs use the earlier slots.
        // We do NOT insert the sentinel into scope_output_names (which is for flat HIR lookups)
        // to avoid IdentifierId conflicts with existing flat HIR instructions.
        let sentinel_var_opt: Option<String> = if self.env.scopes.get(scope_id)
            .and_then(|s| s.early_return_value).is_some()
        {
            let v = format!("t{}", *scope_index + self.param_name_offset);
            *scope_index += 1;
            Some(v)
        } else {
            None
        };
        // Look up the early-return label name for this scope (registered in emit()).
        let early_return_label: Option<String> = if sentinel_var_opt.is_some() {
            self.env.scopes.get(scope_id)
                .and_then(|s| s.early_return_label_id)
                .and_then(|lid| self.switch_fallthrough_labels.get(&lid).cloned())
        } else {
            None
        };

        let intra_set: std::collections::HashSet<usize> =
            analysis.intra_scope_stores.iter().copied().collect();
        let skip_set: std::collections::HashSet<usize> =
            analysis.outputs.iter().filter_map(|o| o.skip_idx).collect();

        let all_out_names: Vec<String> = analysis.outputs.iter()
            .filter(|o| o.is_named_var)
            .filter_map(|o| o.out_name.clone())
            .collect();

        // Collect DeclareLocal instructions from the scope body that should be hoisted
        // to before the scope block. A `let x;` inside the if-block is block-scoped to
        // the if-branch, making `x` inaccessible after the scope. Hoisting ensures the
        // variable is accessible in code after the scope (e.g., for-in loop bodies).
        // Computed after scope_block_set is available (see below).
        let mut hoisted_declare_names: Vec<String> = Vec::new();
        let mut hoisted_declare_skip: std::collections::HashSet<usize> = std::collections::HashSet::new();
        self.hoisted_declare_instr_ids.clear();

        // Emit any pre-scope const promotions.
        for line in &pre_scope_lines {
            let _ = writeln!(out, "{}", line);
        }

        // --- CFG-based scope body emission ---
        // Build the set of instructions and blocks belonging to this scope.
        let scope_instr_set: std::collections::HashSet<InstructionId> =
            instrs.iter().map(|i| i.id).collect();
        let skip_instr_set: std::collections::HashSet<InstructionId> =
            skip_set.iter().filter_map(|&i| instrs.get(i).map(|instr| instr.id)).collect();
        let intra_instr_ids: std::collections::HashSet<InstructionId> =
            intra_set.iter().filter_map(|&i| instrs.get(i).map(|instr| instr.id)).collect();

        // Find the set of blocks containing scope instructions.
        let scope_block_set: std::collections::HashSet<BlockId> = instrs.iter()
            .filter_map(|i| self.instr_to_block.get(&i.id).copied())
            .collect();

        // Compute hoisted DeclareLocal declarations now that scope_block_set is available.
        // A DeclareLocal is hoisted if its variable is used in a block OUTSIDE the scope's blocks.
        {
            let scope_block_instr_ids: std::collections::HashSet<InstructionId> = scope_block_set.iter()
                .filter_map(|bid| self.hir.body.blocks.get(bid))
                .flat_map(|blk| blk.instructions.iter().map(|i| i.id))
                .collect();
            for (i, instr) in instrs.iter().enumerate() {
                if let InstructionValue::DeclareLocal { lvalue, .. } = &instr.value {
                    let decl_id = lvalue.place.identifier;
                    let name = self.env.get_identifier(decl_id)
                        .and_then(|id| id.name.as_ref())
                        .map(|n| n.value().to_string());
                    if let Some(ref n) = name {
                        if !n.starts_with("$t")
                            && !declared_names.contains(n)
                            && !output_cache_vars.contains(n)
                        {
                            // Only hoist if the variable is referenced by an instruction in a block
                            // OUTSIDE the scope's blocks. Inlined instructions in scope blocks are
                            // also "inside" the scope even if not tagged to it.
                            let used_outside = self.hir.body.blocks.values().any(|blk| {
                                blk.instructions.iter().any(|bi| {
                                    if scope_block_instr_ids.contains(&bi.id) { return false; }
                                    match &bi.value {
                                        InstructionValue::LoadLocal { place, .. } => place.identifier == decl_id,
                                        InstructionValue::StoreLocal { lvalue: lv, value, .. } =>
                                            lv.place.identifier == decl_id || value.identifier == decl_id,
                                        InstructionValue::DeclareLocal { lvalue: lv, .. } =>
                                            lv.place.identifier == decl_id,
                                        _ => false,
                                    }
                                })
                            });
                            if used_outside {
                                hoisted_declare_names.push(n.clone());
                                hoisted_declare_skip.insert(i);
                                self.hoisted_declare_instr_ids.insert(instr.id);
                            }
                        }
                    }
                }
            }
        }

        // Find the scope's start block: the scope block with no predecessors from other scope blocks.
        let start_block = if scope_block_set.len() > 1 {
            let mut has_scope_pred: std::collections::HashSet<BlockId> = std::collections::HashSet::new();
            for &bid in &scope_block_set {
                if let Some(block) = self.hir.body.blocks.get(&bid) {
                    for succ in block.terminal.successors() {
                        if scope_block_set.contains(&succ) {
                            has_scope_pred.insert(succ);
                        }
                    }
                }
            }
            scope_block_set.iter()
                .find(|&&bid| !has_scope_pred.contains(&bid))
                .copied()
        } else {
            scope_block_set.iter().next().copied()
        };

        // Set active_early_return BEFORE body_str_opt computation so that
        // emit_scope_body_cfg_walk sees it when handling If/Return terminals.
        self.active_early_return = sentinel_var_opt.as_ref().zip(early_return_label.as_ref())
            .map(|(sv, lbl)| (sv.clone(), lbl.clone()));

        // If tree_body is provided (tree codegen path), use codegen_tree_block for body emission.
        // This correctly handles loop terminals (While, For, etc.) inside the scope body.
        let tree_body_str: Option<String> = if let Some((tree_stmts, tree_scope_instrs)) = tree_body {
            let mut body_out = String::new();
            let mut local_declared = declared_names.clone();
            self.codegen_tree_block(tree_stmts, &all_out_names, indent + 1, &mut body_out, tree_scope_instrs, inlined_ids, scope_index, &mut local_declared);
            Some(body_out)
        } else {
            None
        };

        // If we have multiple scope blocks (scope spans if/else), use CFG-based emission.
        // Otherwise fall back to flat emission (single block, no control flow structure needed).
        // For early-return scopes, always use cfg-walk so that If terminals (with early returns
        // in their branches) are properly emitted even when the scope fits in a single block.
        let body_str_opt: Option<String> = tree_body_str.or_else(|| {
            if scope_block_set.len() > 1 || early_return_label.is_some() {
                if let Some(start_bid) = start_block {
                    let mut body_str = String::new();
                    let mut vis = std::collections::HashSet::new();
                    self.emit_scope_body_cfg_walk(
                        start_bid, &scope_block_set, &scope_instr_set, &intra_instr_ids,
                        &skip_instr_set, inlined_ids, &all_out_names, scope_id,
                        indent + 1, None, &mut vis, &mut body_str,
                    );
                    Some(body_str)
                } else {
                    None  // couldn't find start block, fall back to flat
                }
            } else {
                None  // single block scope without early return, flat emission is correct
            }
        });

        // Build body lines (flat fallback for single-block scopes).
        // Instructions after the last scope-output store should be emitted AFTER
        // the scope output assignment, not before. Split into before/after groups.
        let max_skip_idx = skip_set.iter().copied().max().unwrap_or(0);
        let mut body_lines: Vec<String> = Vec::new();
        let mut post_scope_lines: Vec<String> = Vec::new();
        if body_str_opt.is_none() {
            for (i, instr) in instrs.iter().enumerate() {
                if skip_set.contains(&i) { continue; }
                if hoisted_declare_skip.contains(&i) { continue; }
                if inlined_ids.contains(&instr.lvalue.identifier.0) && !intra_set.contains(&i) {
                    continue;
                }
                // Only StoreLocal instructions go to post_scope_lines (e.g. `const arr = t1;`
                // after the scope block). Side effects (StoreProp, Call, etc.) must stay
                // inside the scope body even if they appear after the main output StoreLocal.
                let is_post_scope_store = i > max_skip_idx
                    && !skip_set.is_empty()
                    && matches!(&instr.value, InstructionValue::StoreLocal { .. });
                let target_list = if is_post_scope_store {
                    &mut post_scope_lines
                } else {
                    &mut body_lines
                };
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
                            // Skip pre-scope null/undefined init: Let-kind StoreLocal for a named
                            // scope output with a null/undefined primitive value. e.g. `let s = null;`
                            // before a scope that reassigns `s = {}` inside. The scope output mechanism
                            // handles `let s;` and `s = $[N];`, so this init is redundant.
                            if matches!(lvalue.kind, InstructionKind::Let | InstructionKind::HoistedLet)
                                && all_out_names.contains(&n)
                                && (val_expr == "null" || val_expr == "undefined")
                            {
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
                            target_list.push(stmt);
                            continue;
                        }
                    }
                }
                if let Some(s) = self.emit_stmt(instr, Some(*scope_id), &all_out_names) {
                    target_list.push(s);
                }
            }
        }

        // How many output slots the early-return sentinel occupies (0 or 1 at position [0]).
        let sentinel_slot_count = sentinel_var_opt.is_some() as usize;

        if has_deps {
            // Emit hoisted dep const declarations.
            for (orig, hoisted) in &hoisted_dep_info {
                if orig != hoisted {
                    let _ = writeln!(out, "{pad}const {hoisted} = {orig};");
                }
            }
            // Emit hoisted DeclareLocal declarations first (they precede the scope logically).
            for name in &hoisted_declare_names {
                let _ = writeln!(out, "{pad}let {name};");
            }
            // Emit `let` for regular outputs, then sentinel last.
            for cache_var in &output_cache_vars {
                if !declared_names.contains(cache_var) {
                    let _ = writeln!(out, "{pad}let {cache_var};");
                }
            }
            if let Some(ref sv) = sentinel_var_opt {
                if !declared_names.contains(sv) {
                    let _ = writeln!(out, "{pad}let {sv};");
                }
            }
            let cond_parts: Vec<String> = hoisted_dep_info.iter().zip(&dep_slot_list)
                .map(|((_, dep_str), &slot)| {
                    format!("$[{slot}] !== {dep_str}")
                })
                .collect();
            let condition = cond_parts.join(" || ");
            let _ = writeln!(out, "{pad}if ({condition}) {{");
            // Emit sentinel init at start of new-value branch.
            if let Some(ref sv) = sentinel_var_opt {
                let _ = writeln!(out, "{body_pad}{sv} = Symbol.for(\"react.early_return_sentinel\");");
            }
            // Emit scope body, wrapped in labeled block when there's an early return.
            if let Some(ref lbl) = early_return_label {
                let inner_pad = "  ".repeat(indent + 2);
                let _ = writeln!(out, "{body_pad}{lbl}: {{");
                if let Some(ref body_str) = body_str_opt {
                    // body_str was generated at indent+1; re-indent to indent+2.
                    for line in body_str.lines() {
                        if line.trim().is_empty() {
                            let _ = writeln!(out, "");
                        } else {
                            let _ = writeln!(out, "  {line}");
                        }
                    }
                } else {
                    for line in &body_lines {
                        let reindented = reindent_multiline(line, &inner_pad);
                        let _ = writeln!(out, "{inner_pad}{reindented}");
                    }
                }
                let _ = writeln!(out, "{body_pad}}}");
            } else if let Some(ref body_str) = body_str_opt {
                out.push_str(body_str);
            } else {
                for line in &body_lines {
                    let reindented = reindent_multiline(line, &body_pad);
                    let _ = writeln!(out, "{body_pad}{reindented}");
                }
            }
            self.active_early_return = None;
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
            // Order: non-reassign declarations first, sentinel second, reassignments last.
            // This matches the TS compiler's cache slot ordering.
            {
                let mut slot_it = out_slot_list.iter().copied();
                let mut ordered: Vec<(String, usize)> = Vec::new();
                if sentinel_var_opt.is_some() {
                    for (v, o) in output_cache_vars.iter().zip(analysis.outputs.iter()) {
                        if !o.is_reassign { if let Some(s) = slot_it.next() { ordered.push((v.clone(), s)); } }
                    }
                    if let Some(ref sv) = sentinel_var_opt {
                        if let Some(s) = slot_it.next() { ordered.push((sv.clone(), s)); }
                    }
                    for (v, o) in output_cache_vars.iter().zip(analysis.outputs.iter()) {
                        if o.is_reassign { if let Some(s) = slot_it.next() { ordered.push((v.clone(), s)); } }
                    }
                } else {
                    for (v, s) in output_cache_vars.iter().zip(slot_it) { ordered.push((v.clone(), s)); }
                }
                for (cache_var, slot) in &ordered {
                    let _ = writeln!(out, "{body_pad}$[{slot}] = {cache_var};");
                }
                let _ = writeln!(out, "{pad}}} else {{");
                for (cache_var, slot) in &ordered {
                    let _ = writeln!(out, "{body_pad}{cache_var} = $[{slot}];");
                }
            }
            let _ = writeln!(out, "{pad}}}");
            for (output, cache_var) in analysis.outputs.iter().zip(&output_cache_vars) {
                if !output.is_named_var {
                    if let Some(ref name) = output.out_name {
                        if output.out_kw.is_empty() {
                            let _ = writeln!(out, "{pad}{name} = {cache_var};");
                        } else {
                            let _ = writeln!(out, "{pad}{} {name} = {cache_var};", output.out_kw);
                        }
                    }
                }
            }
            if body_str_opt.is_none() {
                for line in &post_scope_lines {
                    let reindented = reindent_multiline(line, &pad);
                    let _ = writeln!(out, "{pad}{reindented}");
                }
            }
        } else {
            // Emit hoisted DeclareLocal declarations first (they precede the scope logically).
            for name in &hoisted_declare_names {
                let _ = writeln!(out, "{pad}let {name};");
            }
            // Emit `let` for regular outputs, then sentinel last.
            for cache_var in &output_cache_vars {
                if !declared_names.contains(cache_var) {
                    let _ = writeln!(out, "{pad}let {cache_var};");
                }
            }
            if let Some(ref sv) = sentinel_var_opt {
                if !declared_names.contains(sv) {
                    let _ = writeln!(out, "{pad}let {sv};");
                }
            }
            let _ = writeln!(out, "{pad}if ($[{sentinel_slot}] === Symbol.for(\"react.memo_cache_sentinel\")) {{");
            // Emit sentinel init at start of new-value branch.
            if let Some(ref sv) = sentinel_var_opt {
                let _ = writeln!(out, "{body_pad}{sv} = Symbol.for(\"react.early_return_sentinel\");");
            }
            // Emit scope body, wrapped in labeled block when there's an early return.
            if let Some(ref lbl) = early_return_label {
                let inner_pad = "  ".repeat(indent + 2);
                let _ = writeln!(out, "{body_pad}{lbl}: {{");
                if let Some(ref body_str) = body_str_opt {
                    for line in body_str.lines() {
                        if line.trim().is_empty() {
                            let _ = writeln!(out, "");
                        } else {
                            let _ = writeln!(out, "  {line}");
                        }
                    }
                } else {
                    for line in &body_lines {
                        let reindented = reindent_multiline(line, &inner_pad);
                        let _ = writeln!(out, "{inner_pad}{reindented}");
                    }
                }
                let _ = writeln!(out, "{body_pad}}}");
            } else if let Some(ref body_str) = body_str_opt {
                out.push_str(body_str);
            } else {
                for line in &body_lines {
                    let reindented = reindent_multiline(line, &body_pad);
                    let _ = writeln!(out, "{body_pad}{reindented}");
                }
            }
            self.active_early_return = None;
            for (output, cache_var) in analysis.outputs.iter().zip(&output_cache_vars) {
                if !output.is_named_var {
                    let expr_str = maybe_paren_jsx_scope_output(cache_var, &output.cache_expr);
                    let reindented = reindent_multiline(&expr_str, &body_pad);
                    let _ = writeln!(out, "{body_pad}{cache_var} = {};", reindented);
                }
            }
            let n_regular_slots_nd = out_slot_list.len().saturating_sub(sentinel_slot_count);
            // Regular output cache stores first, sentinel last.
            for (cache_var, &slot) in output_cache_vars.iter().zip(out_slot_list.iter().take(n_regular_slots_nd)) {
                let _ = writeln!(out, "{body_pad}$[{slot}] = {cache_var};");
            }
            // Sentinel cache store at out_slot_list.last().
            if let Some((ref sv, &slot)) = sentinel_var_opt.as_ref().zip(out_slot_list.last()) {
                let _ = writeln!(out, "{body_pad}$[{slot}] = {sv};");
            }
            let _ = writeln!(out, "{pad}}} else {{");
            // Regular output cache loads first, sentinel last.
            for (cache_var, &slot) in output_cache_vars.iter().zip(out_slot_list.iter().take(n_regular_slots_nd)) {
                let _ = writeln!(out, "{body_pad}{cache_var} = $[{slot}];");
            }
            // Sentinel cache load.
            if let Some((ref sv, &slot)) = sentinel_var_opt.as_ref().zip(out_slot_list.last()) {
                let _ = writeln!(out, "{body_pad}{sv} = $[{slot}];");
            }
            let _ = writeln!(out, "{pad}}}");
            for (output, cache_var) in analysis.outputs.iter().zip(&output_cache_vars) {
                if !output.is_named_var {
                    if let Some(ref name) = output.out_name {
                        if name != cache_var {
                            if output.out_kw.is_empty() {
                                let _ = writeln!(out, "{pad}{name} = {cache_var};");
                            } else {
                                let _ = writeln!(out, "{pad}{} {name} = {cache_var};", output.out_kw);
                            }
                        }
                    }
                }
            }
            if body_str_opt.is_none() {
                for line in &post_scope_lines {
                    let reindented = reindent_multiline(line, &pad);
                    let _ = writeln!(out, "{pad}{reindented}");
                }
            }
        }
        // After the scope, emit the early-return check if this scope has a sentinel.
        if let Some(ref sv) = sentinel_var_opt {
            let _ = writeln!(out, "{pad}if ({sv} !== Symbol.for(\"react.early_return_sentinel\")) {{");
            let _ = writeln!(out, "{body_pad}return {sv};");
            let _ = writeln!(out, "{pad}}}");
        }

        // After scope emission, override inlined_exprs for each skipped instruction.
        // Collect the old->new mappings so we can propagate them.
        let mut old_to_new: Vec<(String, String)> = Vec::new();
        for (output, cache_var) in analysis.outputs.iter().zip(&output_cache_vars) {
            if let Some(skip_i) = output.skip_idx {
                if let Some(skip_instr) = instrs.get(skip_i) {
                    let old_name = format!("$t{}", skip_instr.lvalue.identifier.0);
                    old_to_new.push((old_name, cache_var.clone()));
                    // Also add the OLD inlined expression value to old_to_new for
                    // expression-body propagation. If `inlined_exprs[skip_id]` was the full
                    // lambda/expression body (not just a `$tN` reference), other inlined_exprs
                    // entries that contain the same expression body should also be updated to
                    // use the scope output var name (e.g. duplicate FunctionExpression ids).
                    let old_inlined_val = self.inlined_exprs.get(&skip_instr.lvalue.identifier.0).cloned();
                    self.inlined_exprs.insert(skip_instr.lvalue.identifier.0, cache_var.clone());
                    if let Some(old_val) = old_inlined_val {
                        if old_val.as_str() != cache_var.as_str() && !old_val.starts_with("$t") {
                            // old_val is a complex expression (lambda body, etc.) — not just a $tN ref.
                            // Add to old_to_new so the propagation step will update other entries
                            // that have the same expression body.
                            old_to_new.push((old_val, cache_var.clone()));
                        }
                    }
                    // Special case: if the skipped instruction is a Destructure, also map
                    // the Destructure's VALUE to the scope output var. This allows the
                    // post-scope Destructure emission to use `self.expr(value)` → scope var.
                    if let InstructionValue::Destructure { value, .. } = &skip_instr.value {
                        self.inlined_exprs.insert(value.identifier.0, cache_var.clone());
                    }
                }
            }
        }
        // Propagate: update any inlined_exprs entries that still reference old $tN names.
        if !old_to_new.is_empty() {
            // Build a HashMap of id → new_name for $tN-pattern entries, for use with
            // substitute_temp_refs_in_str (handles `$tN` tokens in complex expressions).
            let mut id_subst_map: HashMap<u32, String> = HashMap::new();
            for (old_name, new_name) in &old_to_new {
                if old_name.starts_with("$t") {
                    if let Ok(id) = old_name[2..].parse::<u32>() {
                        id_subst_map.insert(id, new_name.clone());
                    }
                }
            }

            for value in self.inlined_exprs.values_mut() {
                // First: exact match for any old_to_new entry.
                let mut matched = false;
                for (old_name, new_name) in &old_to_new {
                    if value.as_str() == old_name.as_str() {
                        *value = new_name.clone();
                        matched = true;
                        break;
                    }
                }
                if matched { continue; }

                // Second: $tN token substitution for complex expressions.
                if !id_subst_map.is_empty() && value.contains("$t") {
                    let new_val = Self::substitute_temp_refs_in_str(value, &id_subst_map);
                    if new_val != *value {
                        *value = new_val;
                        continue;
                    }
                }

                // Third: for resolved expressions (non-$t), also do substring replacement
                // in complex inlined expressions like "useState({})" → "useState(t0)".
                // This handles cases where the scope output's expression was inlined into
                // a consumer's inlined_exprs entry before scope emission renamed it.
                // Note: don't break — a single value may need multiple substitutions
                // (e.g., "[{}, [], props.value]" needs both {} → t0 and [] → t1).
                for (old_name, new_name) in &old_to_new {
                    if !old_name.starts_with("$t") && value.contains(old_name.as_str()) {
                        *value = value.replace(old_name.as_str(), new_name.as_str());
                    }
                }
            }
        }

        // Emit the post-scope Destructure (if any). This handles cases like
        // `const [[x] = ['default']] = props.y;` where the scope computes the
        // conditional result (TernaryExpression), and the Destructure `const [x] = result`
        // must be emitted AFTER the scope's if/else block.
        if let Some(dest_idx) = analysis.post_scope_destructure_idx {
            if let Some(dest_instr) = instrs.get(dest_idx) {
                if let Some(s) = self.emit_stmt(dest_instr, Some(*scope_id), &all_out_names) {
                    let reindented = reindent_multiline(&s, &pad);
                    let _ = writeln!(out, "{pad}{reindented}");
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Loop-wrapped scope helpers
    // -----------------------------------------------------------------------

    /// Walk the scope body using the CFG structure, emitting scope instructions
    /// with proper control flow (if/else, etc.) instead of flat emission.
    /// Only processes blocks in `scope_block_set` (blocks containing scope instructions).
    /// `stop_at`: if Some(bid), stop before processing that block (used to prevent
    /// if-branch walks from crossing the if's fallthrough block).
    #[allow(clippy::too_many_arguments)]
    fn emit_scope_body_cfg_walk(
        &mut self,
        current: BlockId,
        scope_block_set: &std::collections::HashSet<BlockId>,
        scope_instr_set: &std::collections::HashSet<InstructionId>,
        intra_instr_ids: &std::collections::HashSet<InstructionId>,
        skip_instr_set: &std::collections::HashSet<InstructionId>,
        inlined_ids: &std::collections::HashSet<u32>,
        all_out_names: &[String],
        scope_id: &ScopeId,
        indent: usize,
        stop_at: Option<BlockId>,
        visited: &mut std::collections::HashSet<BlockId>,
        out: &mut String,
    ) {
        // Stop at the designated stop block (e.g. if-fallthrough during branch walk).
        if stop_at == Some(current) { return; }
        // Only process blocks that contain scope instructions.
        if !scope_block_set.contains(&current) { return; }
        if !visited.insert(current) { return; }

        let pad = "  ".repeat(indent);

        let Some(block) = self.hir.body.blocks.get(&current).cloned() else { return; };

        // Emit scope instructions in this block.
        for instr in &block.instructions {
            if !scope_instr_set.contains(&instr.id) { continue; }
            if skip_instr_set.contains(&instr.id) { continue; }
            // Skip DeclareLocal instructions that were hoisted to before the scope block.
            if self.hoisted_declare_instr_ids.contains(&instr.id) { continue; }
            let lv_id = instr.lvalue.identifier.0;
            // Skip inlined-only temporaries unless they're intra-scope StoreLocals.
            if inlined_ids.contains(&lv_id) && !intra_instr_ids.contains(&instr.id) { continue; }
            if let Some(s) = self.emit_stmt(instr, Some(*scope_id), all_out_names) {
                for line in s.lines() {
                    let _ = writeln!(out, "{pad}{}", line);
                }
            }
        }

        // Handle the block's terminal to preserve control flow structure.
        match &block.terminal.clone() {
            Terminal::If { test, consequent, alternate, fallthrough, .. } => {
                let body_indent = indent + 1;
                let body_pad = "  ".repeat(indent);  // same as pad
                let inner_pad = "  ".repeat(body_indent);

                let consq_has = scope_block_set.contains(consequent);
                let alt_has = *alternate != *fallthrough && scope_block_set.contains(alternate);

                // For early-return scopes: also handle branches that lead to Return terminals.
                let (consq_early_ret, alt_early_ret) = if let Some((ref sv, ref lbl)) = self.active_early_return.clone() {
                    let consq_ret = self.early_return_branch_stmt(*consequent, sv, lbl, body_indent);
                    let alt_ret = if *alternate != *fallthrough {
                        self.early_return_branch_stmt(*alternate, sv, lbl, body_indent)
                    } else { None };
                    (consq_ret, alt_ret)
                } else {
                    (None, None)
                };

                // Check for labeled-break pattern: a branch that is a direct Goto to stop_at
                // (the label's fallthrough block). e.g. `if (props.b) { break bb0; }`
                // where the consequent block is just `Goto(label_fallthrough)`.
                let break_label = stop_at.and_then(|sa| self.switch_fallthrough_labels.get(&sa).cloned());
                let consq_is_labeled_break = !consq_has && consq_early_ret.is_none() && {
                    if let Some(sa) = stop_at {
                        self.block_is_direct_goto_to(*consequent, sa)
                    } else { false }
                };
                let alt_is_labeled_break = *alternate != *fallthrough && !alt_has && alt_early_ret.is_none() && {
                    if let Some(sa) = stop_at {
                        self.block_is_direct_goto_to(*alternate, sa)
                    } else { false }
                };

                if consq_has || alt_has || consq_early_ret.is_some() || alt_early_ret.is_some()
                    || (consq_is_labeled_break && break_label.is_some())
                    || (alt_is_labeled_break && break_label.is_some())
                {
                    let test_expr = self.expr(test);
                    // Use fallthrough as the stop point for branch walks so they don't
                    // cross into the shared continuation block.
                    let branch_stop = Some(*fallthrough);

                    let mut if_body = String::new();
                    let mut vis2 = visited.clone();
                    if consq_has {
                        self.emit_scope_body_cfg_walk(
                            *consequent, scope_block_set, scope_instr_set, intra_instr_ids,
                            skip_instr_set, inlined_ids, all_out_names, scope_id,
                            body_indent, branch_stop, &mut vis2, &mut if_body,
                        );
                    } else if let Some(ref stmt) = consq_early_ret {
                        if_body.push_str(stmt);
                    } else if consq_is_labeled_break {
                        if let Some(ref lbl) = break_label {
                            let inner_pad_str = "  ".repeat(body_indent);
                            let _ = writeln!(if_body, "{inner_pad_str}break {lbl};");
                        }
                    }

                    let mut else_body = String::new();
                    let mut vis3 = visited.clone();
                    if alt_has {
                        self.emit_scope_body_cfg_walk(
                            *alternate, scope_block_set, scope_instr_set, intra_instr_ids,
                            skip_instr_set, inlined_ids, all_out_names, scope_id,
                            body_indent, branch_stop, &mut vis3, &mut else_body,
                        );
                    } else if let Some(ref stmt) = alt_early_ret {
                        else_body.push_str(stmt);
                    } else if alt_is_labeled_break {
                        if let Some(ref lbl) = break_label {
                            let inner_pad_str = "  ".repeat(body_indent);
                            let _ = writeln!(else_body, "{inner_pad_str}break {lbl};");
                        }
                    }

                    if !if_body.is_empty() || !else_body.is_empty() {
                        let _ = writeln!(out, "{body_pad}if ({test_expr}) {{");
                        out.push_str(&if_body);
                        if !else_body.is_empty() {
                            let _ = writeln!(out, "{body_pad}}} else {{");
                            out.push_str(&else_body);
                        }
                        let _ = writeln!(out, "{body_pad}}}");
                    }
                }

                // Continue at fallthrough — clear active_early_return so that any Return
                // terminal on the main scope-exit path is NOT transformed (it's the natural
                // scope exit, emitted by the caller after the scope block).
                let saved_er = self.active_early_return.take();
                self.emit_scope_body_cfg_walk(
                    *fallthrough, scope_block_set, scope_instr_set, intra_instr_ids,
                    skip_instr_set, inlined_ids, all_out_names, scope_id,
                    indent, stop_at, visited, out,
                );
                self.active_early_return = saved_er;
            }
            Terminal::Goto { block: next, .. } => {
                self.emit_scope_body_cfg_walk(
                    *next, scope_block_set, scope_instr_set, intra_instr_ids,
                    skip_instr_set, inlined_ids, all_out_names, scope_id,
                    indent, stop_at, visited, out,
                );
            }
            Terminal::DoWhile { loop_, fallthrough, .. }
            | Terminal::While { loop_, fallthrough, .. } => {
                // Walk the loop body (it's always executed at least once for do-while,
                // and may have scope instructions). Use a separate visited set for the
                // loop body to avoid marking the fallthrough as visited prematurely.
                let mut loop_vis = visited.clone();
                self.emit_scope_body_cfg_walk(
                    *loop_, scope_block_set, scope_instr_set, intra_instr_ids,
                    skip_instr_set, inlined_ids, all_out_names, scope_id,
                    indent, Some(*fallthrough), &mut loop_vis, out,
                );
                // Continue at fallthrough.
                self.emit_scope_body_cfg_walk(
                    *fallthrough, scope_block_set, scope_instr_set, intra_instr_ids,
                    skip_instr_set, inlined_ids, all_out_names, scope_id,
                    indent, stop_at, visited, out,
                );
            }
            Terminal::ForOf { loop_, fallthrough, .. }
            | Terminal::ForIn { loop_, fallthrough, .. } => {
                let mut loop_vis = visited.clone();
                self.emit_scope_body_cfg_walk(
                    *loop_, scope_block_set, scope_instr_set, intra_instr_ids,
                    skip_instr_set, inlined_ids, all_out_names, scope_id,
                    indent, Some(*fallthrough), &mut loop_vis, out,
                );
                self.emit_scope_body_cfg_walk(
                    *fallthrough, scope_block_set, scope_instr_set, intra_instr_ids,
                    skip_instr_set, inlined_ids, all_out_names, scope_id,
                    indent, stop_at, visited, out,
                );
            }
            Terminal::Label { block: body, fallthrough, .. } => {
                let body_bid = *body;
                let fall_bid = *fallthrough;
                if stop_at == Some(fall_bid) {
                    // If fallthrough is our stop point, just walk the label body.
                    if scope_block_set.contains(&body_bid) {
                        let label_opt = self.switch_fallthrough_labels.get(&fall_bid).cloned();
                        if let Some(ref label) = label_opt {
                            let _ = writeln!(out, "{pad}{label}: {{");
                            let mut vis2 = visited.clone();
                            self.emit_scope_body_cfg_walk(
                                body_bid, scope_block_set, scope_instr_set, intra_instr_ids,
                                skip_instr_set, inlined_ids, all_out_names, scope_id,
                                indent + 1, Some(fall_bid), &mut vis2, out,
                            );
                            let _ = writeln!(out, "{pad}}}");
                        } else {
                            let mut vis2 = visited.clone();
                            self.emit_scope_body_cfg_walk(
                                body_bid, scope_block_set, scope_instr_set, intra_instr_ids,
                                skip_instr_set, inlined_ids, all_out_names, scope_id,
                                indent, Some(fall_bid), &mut vis2, out,
                            );
                        }
                        self.scope_emitted_label_bodies.insert(body_bid);
                    }
                    return;
                }
                // Walk the label body (with label wrapping if one exists).
                let label_opt = self.switch_fallthrough_labels.get(&fall_bid).cloned();
                if scope_block_set.contains(&body_bid) {
                    if let Some(ref label) = label_opt {
                        let _ = writeln!(out, "{pad}{label}: {{");
                        let mut vis2 = visited.clone();
                        self.emit_scope_body_cfg_walk(
                            body_bid, scope_block_set, scope_instr_set, intra_instr_ids,
                            skip_instr_set, inlined_ids, all_out_names, scope_id,
                            indent + 1, Some(fall_bid), &mut vis2, out,
                        );
                        let _ = writeln!(out, "{pad}}}");
                    } else {
                        let mut vis2 = visited.clone();
                        self.emit_scope_body_cfg_walk(
                            body_bid, scope_block_set, scope_instr_set, intra_instr_ids,
                            skip_instr_set, inlined_ids, all_out_names, scope_id,
                            indent, Some(fall_bid), &mut vis2, out,
                        );
                    }
                    self.scope_emitted_label_bodies.insert(body_bid);
                }
                // Continue at fallthrough.
                self.emit_scope_body_cfg_walk(
                    fall_bid, scope_block_set, scope_instr_set, intra_instr_ids,
                    skip_instr_set, inlined_ids, all_out_names, scope_id,
                    indent, stop_at, visited, out,
                );
            }
            // If inside an early-return scope and we see a Return terminal directly,
            // transform it: sentinel = val; break label.
            Terminal::Return { value, .. } => {
                if let Some((ref sv, ref lbl)) = self.active_early_return.clone() {
                    let val_expr = self.expr(value);
                    let _ = writeln!(out, "{pad}{sv} = {val_expr};");
                    let _ = writeln!(out, "{pad}break {lbl};");
                    // Mark this block as handled so emit_cfg_region doesn't re-emit it.
                    self.early_return_handled_blocks.insert(current);
                }
                // Else: stop walking (natural scope exit, handled after scope block).
            }
            // For other terminals (Throw, etc.), stop walking.
            _ => {}
        }
    }

    /// For an early-return branch: if `bid` directly leads to a Return terminal
    /// (possibly through a chain of empty Goto blocks), return the transformed statement.
    /// Returns true if the block at `bid` is an empty block whose only terminal is
    /// `Goto { block: target }`. Used to detect labeled-break patterns.
    fn block_is_direct_goto_to(&self, bid: BlockId, target: BlockId) -> bool {
        if let Some(block) = self.hir.body.blocks.get(&bid) {
            if block.instructions.is_empty() && block.phis.is_empty() {
                if let Terminal::Goto { block: next, .. } = &block.terminal {
                    return *next == target;
                }
            }
        }
        false
    }

    fn early_return_branch_stmt(&mut self, bid: BlockId, sentinel_var: &str, label: &str, indent: usize) -> Option<String> {
        let pad = "  ".repeat(indent);
        let mut current = bid;
        let mut visited = std::collections::HashSet::new();
        loop {
            if !visited.insert(current) { return None; }
            let block = self.hir.body.blocks.get(&current)?.clone();
            // Emit any instructions in this block (e.g., const x = val before the return).
            let mut stmts = String::new();
            for instr in &block.instructions {
                if let Some(s) = self.emit_stmt(instr, None, &[]) {
                    let _ = writeln!(stmts, "{pad}{s}");
                }
            }
            match &block.terminal {
                Terminal::Return { value, .. } => {
                    let val_expr = self.expr(value);
                    let _ = writeln!(stmts, "{pad}{sentinel_var} = {val_expr};");
                    let _ = writeln!(stmts, "{pad}break {label};");
                    return Some(stmts);
                }
                Terminal::Goto { block: next, .. } => {
                    current = *next;
                    // For simplicity, only follow empty Goto blocks.
                    if !stmts.trim().is_empty() { return None; }
                }
                _ => return None,
            }
        }
    }

    /// Returns true if any block reachable from `start` (stopping before `stop`)
    /// has at least one instruction. Used to distinguish genuinely empty if-bodies
    /// (no instructions in the HIR) from memoization-emptied ones.
    fn cfg_region_has_instructions(&self, start: BlockId, stop: BlockId) -> bool {
        let mut visited = std::collections::HashSet::new();
        let mut queue = vec![start];
        while let Some(bid) = queue.pop() {
            if bid == stop { continue; }
            if !visited.insert(bid) { continue; }
            if let Some(block) = self.hir.body.blocks.get(&bid) {
                if !block.instructions.is_empty() {
                    return true;
                }
                for succ in block.terminal.successors() {
                    queue.push(succ);
                }
            }
        }
        false
    }

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
                let test_expr = self.do_while_test_expr(test_bid);

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
                let for_of_pattern = self.try_inline_for_of_destructure(
                    loop_bid, iter_next_id, &loop_var_name, inlined_ids,
                );
                let _ = writeln!(out, "{loop_body_pad}for (const {for_of_pattern} of {iterable_expr}) {{");
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

    /// Resolve phi nodes that result from logical expressions (&&, ||, ??) into
    /// inlined JS expressions. Must be called AFTER build_inline_map so that
    /// the phi operand places already have entries in inlined_exprs.
    fn resolve_logical_phis(&mut self) {
        use crate::hir::hir::{Terminal, LogicalOperator};

        // Collect all logical-phi resolutions first (avoid borrow conflict).
        let mut new_entries: Vec<(u32, String)> = Vec::new();
        // Track IDs added by this function so the post-processing substitution
        // only operates on these entries (not entries from build_inline_map).
        let mut logical_phi_ids: std::collections::HashSet<u32> = std::collections::HashSet::new();

        // We may need multiple passes for nested logical expressions (a && b && c).
        // Each pass may expose new phi results that become operands for the next phi.
        for _round in 0..16 {
            let mut added_this_round = 0;

            for (_, block) in &self.hir.body.blocks {
                // Only process blocks that are join points after a logical Branch.
                // We identify these by finding blocks whose predecessor has
                // Terminal::Branch { logical_op: Some(op), fallthrough: this_block }.
                // We'll scan all blocks for such terminals.
                let _ = block; // placeholder; we iterate blocks below
            }

            // Scan all blocks for Terminal::Branch { logical_op: Some(op), .. }
            let block_ids: Vec<crate::hir::hir::BlockId> =
                self.hir.body.blocks.keys().copied().collect();

            for bid in &block_ids {
                let Some(block) = self.hir.body.blocks.get(bid) else { continue };
                let Terminal::Branch {
                    test,
                    consequent,
                    alternate,
                    fallthrough,
                    logical_op: Some(op),
                    ..
                } = &block.terminal else { continue };

                let op = *op;
                let test_place = test.clone();
                let consequent_bid = *consequent;
                let alternate_bid = *alternate;
                let fallthrough_bid = *fallthrough;
                let left_block_id = *bid;

                // The right arm is the block that evaluates the RHS of the operator.
                // For &&: right arm = consequent (evaluate right only if left truthy)
                // For ||/??:  right arm = alternate (evaluate right only if left falsy)
                let right_arm_bid = match op {
                    LogicalOperator::And => consequent_bid,
                    LogicalOperator::Or | LogicalOperator::NullishCoalescing => alternate_bid,
                };

                // Look at the fallthrough block's phis.
                let Some(fall_block) = self.hir.body.blocks.get(&fallthrough_bid) else { continue };

                for phi in &fall_block.phis {
                    // Skip if already resolved.
                    if self.inlined_exprs.contains_key(&phi.place.identifier.0)
                        || new_entries.iter().any(|(id, _)| *id == phi.place.identifier.0)
                    {
                        continue;
                    }

                    // The phi should have an operand from left_block_id (the left value)
                    // and an operand from right_arm_bid (the right value).
                    let left_op = phi.operands.get(&left_block_id);
                    let right_op = phi.operands.get(&right_arm_bid);

                    let (Some(left_place), Some(right_place)) = (left_op, right_op) else {
                        continue;
                    };

                    let left_expr = self.expr_for_phi_operand(left_place);
                    let right_expr = self.expr_for_phi_operand(right_place);

                    // Skip if operands aren't resolved yet (still raw $tN).
                    // They may get resolved in a later round.
                    let op_str = match op {
                        LogicalOperator::And => "&&",
                        LogicalOperator::Or => "||",
                        LogicalOperator::NullishCoalescing => "??",
                    };

                    // Add parentheses around a logical sub-expression when the left operand
                    // uses a DIFFERENT logical operator than the outer expression.
                    // Same-operator chains (e.g. `a && b && c`) don't need parens due to
                    // left-associativity. But different operators always get parens to be
                    // explicit (matching TS compiler style):
                    //   - `(a && b) || c` → parens added (different operators)
                    //   - `(a || b) && c` → parens added (different operators)
                    //   - `a && b && c` → no parens (same operator, left-assoc)
                    let left_has_different_logical = match op {
                        LogicalOperator::And => {
                            left_expr.contains(" || ") || left_expr.contains(" ?? ")
                        }
                        LogicalOperator::Or => {
                            left_expr.contains(" && ") || left_expr.contains(" ?? ")
                        }
                        LogicalOperator::NullishCoalescing => {
                            left_expr.contains(" && ") || left_expr.contains(" || ")
                        }
                    };
                    let left_expr = if left_has_different_logical {
                        format!("({left_expr})")
                    } else {
                        left_expr
                    };

                    let combined = format!("{left_expr} {op_str} {right_expr}");
                    logical_phi_ids.insert(phi.place.identifier.0);
                    // Insert immediately so later entries in the same round can use this result.
                    self.inlined_exprs.insert(phi.place.identifier.0, combined.clone());
                    // Also map any use of the pre-SSA phi result to the combined expression.
                    // Because enter_ssa doesn't rename pre-existing phi results, the terminal
                    // that uses the phi result may have a different SSA id from phi.place.identifier.
                    // Specifically, the fallthrough block's Branch.test or If.test is the
                    // SSA-renamed version.
                    let fall_test_id = match &fall_block.terminal {
                        Terminal::Branch { test: fall_test, logical_op: None, .. } => {
                            Some(fall_test.identifier.0)
                        }
                        Terminal::If { test: fall_test, .. } => {
                            Some(fall_test.identifier.0)
                        }
                        _ => None,
                    };
                    if let Some(ft_id) = fall_test_id {
                        if ft_id != phi.place.identifier.0 {
                            logical_phi_ids.insert(ft_id);
                            self.inlined_exprs.insert(ft_id, combined);
                        }
                    }
                    added_this_round += 1;
                }
            }

            // Ternary phi resolution: Terminal::Branch { logical_op: None } creates
            // ternary expressions `test ? consequent : alternate`. The phi result at the
            // fallthrough block should be emitted as `test ? consq_val : alt_val`.
            for bid in &block_ids {
                let Some(block) = self.hir.body.blocks.get(bid) else { continue };
                let Terminal::Branch {
                    test,
                    consequent,
                    alternate,
                    fallthrough,
                    logical_op: None,
                    ..
                } = &block.terminal else { continue };

                let test_expr = self.expr_for_phi_operand(test);
                let consq_bid = *consequent;
                let alt_bid = *alternate;
                let fallthrough_bid = *fallthrough;
                let branch_bid = *bid;

                let Some(fall_block) = self.hir.body.blocks.get(&fallthrough_bid) else { continue };

                for phi in &fall_block.phis {
                    if self.inlined_exprs.contains_key(&phi.place.identifier.0)
                        || new_entries.iter().any(|(id, _)| *id == phi.place.identifier.0)
                    {
                        continue;
                    }

                    // Only handle 2-operand phis (one from each arm).
                    if phi.operands.len() != 2 { continue; }

                    // Get the operand from each arm. The phi must have exactly one
                    // operand from consequent side and one from alternate side.
                    // The consequent/alternate blocks may have been processed (goto
                    // chains), so we look for any operand NOT from branch_bid itself.
                    let consq_op = phi.operands.get(&consq_bid)
                        .or_else(|| phi.operands.iter()
                            .filter(|(&k, _)| k != branch_bid && k != alt_bid && k != fallthrough_bid)
                            .map(|(_, v)| v).next());
                    let alt_op = phi.operands.get(&alt_bid)
                        .or_else(|| phi.operands.iter()
                            .filter(|(&k, _)| k != branch_bid && k != consq_bid && k != fallthrough_bid)
                            .map(|(_, v)| v).next());

                    let (Some(consq_place), Some(alt_place)) = (consq_op, alt_op) else { continue };

                    let consq_expr = self.expr_for_phi_operand(consq_place);
                    let alt_expr = self.expr_for_phi_operand(alt_place);

                    // Only resolve if operands are fully resolved (no raw $tN).
                    if consq_expr.contains("$t") || alt_expr.contains("$t") { continue; }

                    // Wrap consequent in parens if it contains a ternary (nested ternary in
                    // consequent position needs parens to preserve right-associativity).
                    let consq_expr = if consq_expr.contains(" ? ") {
                        format!("({consq_expr})")
                    } else {
                        consq_expr
                    };
                    // Wrap alternate in parens if it contains ?? (nullish coalescing in
                    // alternate position; TS compiler adds parens for clarity).
                    let alt_expr = if alt_expr.contains(" ?? ") {
                        format!("({alt_expr})")
                    } else {
                        alt_expr
                    };

                    let combined = format!("{test_expr} ? {consq_expr} : {alt_expr}");
                    logical_phi_ids.insert(phi.place.identifier.0);
                    // Insert immediately so later entries in the same round can use this result.
                    self.inlined_exprs.insert(phi.place.identifier.0, combined);
                    added_this_round += 1;
                }
            }

            // Entries were already inserted into inlined_exprs immediately above.
            new_entries.clear(); // no-op but kept for clarity

            if added_this_round == 0 {
                break;
            }
        }

        // Post-processing: substitute any raw $tN references that appear in combined
        // expressions added by resolve_logical_phis. This handles chained logicals like
        // `a && b && c` where the intermediate phi result `$t10` may still appear
        // literally in `$t12`'s string because `$t10` was added to new_entries in the
        // same round that `$t12` was processed (before it was flushed to inlined_exprs).
        //
        // We ONLY substitute within entries that resolve_logical_phis itself added
        // (tracked in logical_phi_ids). We do NOT touch entries from build_inline_map
        // because those may intentionally contain `$tN` references to multi-use SSA
        // temps that should remain as named variables.
        for _ in 0..16 {
            let mut changed = false;
            for &id in &logical_phi_ids {
                let expr = match self.inlined_exprs.get(&id) {
                    Some(e) => e.clone(),
                    None => continue,
                };
                if !expr.contains("$t") {
                    continue;
                }
                let new_expr = Self::substitute_temp_refs_in_str(&expr, &self.inlined_exprs);
                if new_expr != expr {
                    self.inlined_exprs.insert(id, new_expr);
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }

        // Mark instructions from logical-op Branch blocks as inlined.
        // When a block ends with `Branch { logical_op: Some(_) }`, all its instructions
        // are logically part of the compound condition (e.g. `A && B`). They should
        // not be emitted as standalone statements; instead they're embedded via the
        // inlined_exprs for the phi result.
        //
        // We only suppress instructions that are pure expressions (inlinable via
        // try_inline_instr). This avoids suppressing side-effecting instructions
        // that happen to live in the same block.
        let logical_branch_block_ids: Vec<crate::hir::hir::BlockId> =
            self.hir.body.blocks.iter()
                .filter_map(|(&bid, block)| {
                    if matches!(&block.terminal, Terminal::Branch { logical_op: Some(_), .. }) {
                        Some(bid)
                    } else {
                        None
                    }
                })
                .collect();

        for bid in logical_branch_block_ids {
            let block = match self.hir.body.blocks.get(&bid) {
                Some(b) => b,
                None => continue,
            };
            let instrs = block.instructions.clone();
            for instr in &instrs {
                let id = instr.lvalue.identifier.0;
                if self.inlined_exprs.contains_key(&id) { continue; }
                // Only inline if we can compute the expression without side effects.
                if let Some(expr) = self.try_inline_instr(instr) {
                    self.inlined_exprs.insert(id, expr);
                }
            }
        }
    }

    /// Replace occurrences of `$tN` in `expr` with their resolved strings from `map`,
    /// wrapping the resolved expression in parens only when operator precedence requires it.
    ///
    /// Parens are needed when the resolved expression's operators have lower precedence
    /// than the surrounding context:
    /// - `||` or `??` inside `&&` context needs parens
    /// - `??` inside `||` context needs parens
    /// - `&&` inside `&&`, `||`, or `??` context does NOT need parens (higher prec or same)
    fn substitute_temp_refs_in_str(expr: &str, map: &HashMap<u32, String>) -> String {
        let mut result = String::with_capacity(expr.len());
        let bytes = expr.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'$' && i + 1 < bytes.len() && bytes[i + 1] == b't' {
                let digit_start = i + 2;
                let mut j = digit_start;
                while j < bytes.len() && bytes[j].is_ascii_digit() {
                    j += 1;
                }
                if j > digit_start {
                    let id_str = &expr[digit_start..j];
                    if let Ok(id) = id_str.parse::<u32>() {
                        if let Some(resolved) = map.get(&id) {
                            let inner_has_or = resolved.contains(" || ");
                            let inner_has_and = resolved.contains(" && ");
                            let inner_has_nc = resolved.contains(" ?? ");
                            let inner_has_ternary = resolved.contains(" ? ");
                            let after = &expr[j..];
                            // Determine outer operator from context (before and after $tN)
                            let outer_is_and = result.ends_with(" && ") || after.starts_with(" && ");
                            let outer_is_or  = result.ends_with(" || ") || after.starts_with(" || ");
                            let outer_is_nc  = result.ends_with(" ?? ") || after.starts_with(" ?? ");
                            // Check for arithmetic operators in context (+ - * / etc.)
                            let outer_has_arith = {
                                let trimmed_before = result.trim_end_matches(' ');
                                trimmed_before.ends_with('+') || trimmed_before.ends_with('-')
                                    || trimmed_before.ends_with('*') || trimmed_before.ends_with('/')
                                    || trimmed_before.ends_with('%')
                            };
                            // Need parens when inner operator has lower (or different)
                            // precedence than outer. Also add for && inside || for clarity,
                            // matching the TS compiler's behavior of preserving user parens:
                            // - || or ?? inside && context (semantic need)
                            // - && or ?? inside || context (&& for clarity, ?? semantic need)
                            // - && or || inside ?? context (can't mix with ??)
                            // - ternary inside any binary operator context (lowest precedence)
                            // - ternary as consequent of another ternary: `x ? $tN : y` where $tN is ternary
                            let outer_is_ternary_consequent = result.ends_with(" ? ");
                            let needs_parens =
                                (outer_is_and && (inner_has_or || inner_has_nc))
                                || (outer_is_or && (inner_has_and || inner_has_nc))
                                || (outer_is_nc && (inner_has_and || inner_has_or))
                                || (inner_has_ternary && (outer_is_and || outer_is_or || outer_is_nc || outer_has_arith || outer_is_ternary_consequent));
                            if needs_parens {
                                result.push('(');
                                result.push_str(resolved);
                                result.push(')');
                            } else {
                                result.push_str(resolved);
                            }
                            i = j;
                            continue;
                        }
                    }
                }
            }
            // Not a $tN pattern or not in map — copy character as-is.
            // SAFETY: We index into valid UTF-8 bytes at positions produced by
            // byte-by-byte scanning; all non-`$t` paths emit exactly one byte here.
            result.push(bytes[i] as char);
            i += 1;
        }
        result
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
                // Wrap consequent in parens if it contains a nested ternary, to
                // match TS compiler output: `test ? (a ? b : c) : alt`
                // Also wrap TypeCastExpression (`x as T`) in parens in ternary branches,
                // to match TS compiler output: `test ? ("pending" as Status) : t1`
                let c_wrapped = if c.contains(" ? ") || c.contains(" as ") {
                    format!("({c})")
                } else {
                    c
                };
                let a_wrapped = if a.contains(" as ") {
                    format!("({a})")
                } else {
                    a
                };
                Some(format!("{t} ? {c_wrapped} : {a_wrapped}"))
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
                    JsxTag::Place(p) => {
                        let raw = self.expr(p);
                        // Resolve namespace aliases: if tag is "MyLocal.Text" and
                        // "MyLocal" is an alias for a namespace import "SharedRuntime",
                        // emit "SharedRuntime.Text" instead.
                        if let Some(dot_pos) = raw.find('.') {
                            let prefix = &raw[..dot_pos];
                            if let Some(ns_name) = self.namespace_alias.get(prefix).cloned() {
                                format!("{}{}", ns_name, &raw[dot_pos..])
                            } else {
                                raw
                            }
                        } else {
                            raw
                        }
                    }
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
                            } else if inner.contains('\\') {
                                // Any backslash escape (\\n, \\t, \\\\, etc.) would be
                                // misinterpreted in a JSX string attribute. Use JS expression.
                                format!("{name}={{{val}}}")
                            } else {
                                // Plain string without any backslashes: safe as bare string attr.
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
                    let mut src = apply_capture_renames(src, &lowered_func.func.context, self.env, &self.name_overrides);
                    // Apply namespace alias substitutions in the function body source.
                    for (alias_name, ns_name) in &self.namespace_alias {
                        src = rename_namespace_in_src(&src, alias_name, ns_name);
                    }
                    // Normalize: arrow functions with a single unparenthesized param get parens added.
                    // e.g. `e => ...` → `(e) => ...`  (TS compiler always parenthesizes)
                    // Also normalize body text: single quotes → double, computed property → dot.
                    if matches!(fn_type, FunctionExpressionType::Arrow) {
                        let normalized = normalize_arrow_params(&src);
                        let normalized = normalize_fn_body_text(&normalized);
                        let normalized = normalize_jsx_self_closing(&normalized);
                        let normalized = normalize_arrow_expr_body(&normalized);
                        return Some(normalized);
                    }
                    return Some(normalize_jsx_self_closing(&normalize_fn_body_text(&src)));
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

    /// Scan all non-inlined instructions for $t/$T promoted temp identifiers and
    /// assign sequential t0, t1, ... names to them. This mirrors the reference
    /// compiler's rename_variables pass, which renames these temps before codegen.
    /// Returns the count of temps assigned (used to initialize scope_index).
    fn build_promoted_temp_names(
        &mut self,
        ordered: &[Instruction],
        inlined_ids: &std::collections::HashSet<u32>,
        instr_scope: &HashMap<crate::hir::hir::InstructionId, ScopeId>,
    ) -> usize {
        use crate::hir::hir::IdentifierName;

        // Collect scope declaration identifier IDs — only for scopes that are actually
        // emitted (appear in instr_scope). Pruned/merged scopes still exist in self.env.scopes
        // but their declarations should be renamed like normal temps.
        let active_scope_ids: std::collections::HashSet<ScopeId> =
            instr_scope.values().copied().collect();
        let scope_decl_ids: std::collections::HashSet<u32> = self.env.scopes.iter()
            .filter(|(sid, _)| active_scope_ids.contains(sid))
            .flat_map(|(_, s)| s.declarations.keys().map(|id| id.0))
            .collect();

        // Start counter after param slots so body temps don't collide with promoted params.
        // e.g. if param_name_offset=1 (one param named t0), body temps start at t1.
        let mut counter: usize = self.param_name_offset;
        // Track which DeclarationIds have already been assigned a name (to give
        // all SSA copies of the same variable the same tN name).
        let mut decl_seen: std::collections::HashMap<crate::hir::hir::DeclarationId, String> = std::collections::HashMap::new();

        // Helper to try assigning a name to an identifier.
        // `rename_none`: when true, also rename anonymous (name: None) identifiers.
        //   Used for Destructure pattern items which have no name but appear in output.
        let mut assign = |id: crate::hir::hir::IdentifierId,
                          rename_none: bool,
                          counter: &mut usize,
                          promoted_temp_names: &mut HashMap<u32, String>,
                          decl_seen: &mut std::collections::HashMap<crate::hir::hir::DeclarationId, String>| {
            if promoted_temp_names.contains_key(&id.0) { return; }
            if scope_decl_ids.contains(&id.0) { return; }
            let ident = match self.env.get_identifier(id) {
                Some(i) => i.clone(),
                None => return,
            };
            // Only rename:
            //   - Promoted names starting with $t or $T
            //   - Anonymous (name: None) temps when rename_none=true (Destructure pattern items)
            // Named user variables (Named("x")) are NEVER renamed.
            let should_rename = match &ident.name {
                None => rename_none,
                Some(IdentifierName::Promoted(n)) => n.starts_with("$t") || n.starts_with("$T"),
                Some(IdentifierName::Named(_)) => false,
            };
            if !should_rename { return; }
            let is_jsx = ident.name.as_ref().map(|n| n.value().starts_with("$T")).unwrap_or(false);
            let decl_id = ident.declaration_id;
            // If we've already assigned a name for this DeclarationId, reuse it.
            let new_name = if let Some(existing) = decl_seen.get(&decl_id) {
                existing.clone()
            } else {
                let n = if is_jsx {
                    format!("T{}", *counter)
                } else {
                    format!("t{}", *counter)
                };
                *counter += 1;
                decl_seen.insert(decl_id, n.clone());
                n
            };
            promoted_temp_names.insert(id.0, new_name);
        };

        for instr in ordered {
            let lv_id = instr.lvalue.identifier;
            let is_inlined = inlined_ids.contains(&lv_id.0);
            let is_destructure = matches!(&instr.value, InstructionValue::Destructure { .. });
            // For non-inlined, non-Destructure instructions, assign a name to the main lvalue.
            // Destructure instructions don't use their main lvalue in output — only their pattern
            // items appear (e.g. `const [t0] = x` — the main lvalue is unused).
            if !is_inlined && !is_destructure {
                // rename_none=false: only rename Promoted("$t...") names, not anonymous temps
                assign(lv_id, false, &mut counter, &mut self.promoted_temp_names, &mut decl_seen);
            }
            // For Destructure instructions, always scan pattern bindings (even if the
            // instruction's main lvalue is inlined — the pattern items still appear in output).
            if let InstructionValue::Destructure { lvalue, .. } = &instr.value {
                match &lvalue.pattern {
                    crate::hir::hir::Pattern::Array(ap) => {
                        for elem in &ap.items {
                            let pid = match elem {
                                crate::hir::hir::ArrayElement::Place(p) => p.identifier,
                                crate::hir::hir::ArrayElement::Spread(s) => s.place.identifier,
                                crate::hir::hir::ArrayElement::Hole => continue,
                            };
                            // rename_none=true: pattern items may be anonymous and need names
                            assign(pid, true, &mut counter, &mut self.promoted_temp_names, &mut decl_seen);
                        }
                    }
                    crate::hir::hir::Pattern::Object(op) => {
                        for prop in &op.properties {
                            let pid = match prop {
                                crate::hir::hir::ObjectPatternProperty::Property(p) => p.place.identifier,
                                crate::hir::hir::ObjectPatternProperty::Spread(s) => s.place.identifier,
                            };
                            // rename_none=true: pattern items may be anonymous and need names
                            assign(pid, true, &mut counter, &mut self.promoted_temp_names, &mut decl_seen);
                        }
                    }
                }
            }
        }
        // Return the count of body temps assigned (not the final counter value).
        // scope_index starts at this count, and scope temps are t{scope_index + param_name_offset}.
        counter - self.param_name_offset
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
                let value_instr = instrs.iter().find(|i| i.lvalue.identifier == value.identifier);
                let mut value_expr = value_instr
                    .and_then(|vi| self.try_inline_instr(vi))
                    .unwrap_or_else(|| self.expr(value));
                // Method shorthand bodies (e.g. `() { return x; }` from `y() {...}`) start with
                // `(` but lack the `function` keyword — they are only valid inside object literals.
                // When emitted as standalone scope-output expressions, prepend `function `.
                if let Some(vi) = value_instr {
                    if let InstructionValue::FunctionExpression { fn_type: FunctionExpressionType::Expression, lowered_func, .. } = &vi.value {
                        let async_kw = if lowered_func.func.async_ { "async " } else { "" };
                        if value_expr.starts_with('(') && !value_expr.contains("=>") {
                            value_expr = format!("{async_kw}function {value_expr}");
                        }
                    }
                }
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
                // Also check if the instruction's result lvalue (a temp) is used outside scope.
                // This covers the case where the StoreLocal result flows to a scope cache slot:
                // e.g. `const onClick = () => {...}` where the StoreLocal result $t6 is the
                // cache value, even though `onClick` itself isn't directly used outside scope.
                // Treating it as "escaping" ensures we emit `const onClick = tN;` post-scope,
                // which inline_scope_output_names then rewrites to use `onClick` as the temp name.
                if !used_outside && var_name.is_some() {
                    used_outside = self.is_var_used_outside_scope(instr.lvalue.identifier, &scope_lvalue_ids);
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
                let esc_is_reassign = esc_lvalue_kind.map(|k| matches!(k, InstructionKind::Reassign)).unwrap_or(false);
                let output = if is_let_kind || used_after || captured_and_called {
                    ScopeOutputItem {
                        skip_idx: None,
                        cache_expr: esc_name.clone().unwrap_or_else(|| "undefined".to_string()),
                        out_name: esc_name.clone(),
                        out_kw: "let",
                        is_named_var: true,
                        is_reassign: esc_is_reassign,
                    }
                } else {
                    ScopeOutputItem {
                        skip_idx: Some(*esc_idx),
                        cache_expr: esc_value_expr.clone(),
                        out_name: esc_name.clone(),
                        out_kw: "const",
                        is_named_var: false,
                        is_reassign: esc_is_reassign,
                    }
                };
                return ScopeOutput { outputs: vec![output], intra_scope_stores: esc_intra, terminal_place_id: None, terminal_type_cast_annotation: None, post_scope_destructure_idx: None };
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
                        is_reassign: false,
                    }],
                    intra_scope_stores,
                    terminal_place_id: Some(feed_id),
                    terminal_type_cast_annotation: None,
                    post_scope_destructure_idx: None,
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
                        is_reassign: false,
                    }],
                intra_scope_stores,
                terminal_place_id: Some(feed_id),
                terminal_type_cast_annotation: type_cast_ann,
                post_scope_destructure_idx: None,
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
                let is_reassign = lvalue_kind.map(|k| matches!(k, InstructionKind::Reassign)).unwrap_or(false);
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
                        is_reassign,
                    });
                } else {
                    outputs.push(ScopeOutputItem {
                        skip_idx: Some(*idx),
                        cache_expr: value_expr.clone(),
                        out_name: name.clone(),
                        out_kw: "const",
                        is_named_var: false,
                        is_reassign,
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
            return ScopeOutput { outputs, intra_scope_stores: intra, terminal_place_id: None, terminal_type_cast_annotation: None, post_scope_destructure_idx: None };
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
            let mut cache_expr = if let Some(computed) = self.try_inline_instr(instr) {
                computed
            } else if let Some(inlined) = self.inlined_exprs.get(&instr.lvalue.identifier.0) {
                inlined.clone()
            } else {
                self.expr(&instr.lvalue)
            };
            // Method shorthand bodies (e.g. `() { return x; }`) start with `(` but
            // lack `function` keyword — prepend it for valid standalone emission.
            if let InstructionValue::FunctionExpression { fn_type: FunctionExpressionType::Expression, lowered_func, .. } = &instr.value {
                let async_kw = if lowered_func.func.async_ { "async " } else { "" };
                if cache_expr.starts_with('(') && !cache_expr.contains("=>") {
                    cache_expr = format!("{async_kw}function {cache_expr}");
                }
            }
            multi_outputs.push(ScopeOutputItem {
                skip_idx: Some(idx),
                cache_expr,
                out_name: None,
                out_kw: "const",
                is_named_var: false,
                        is_reassign: false,
                    });
        }
        if !multi_outputs.is_empty() {
            return ScopeOutput {
                outputs: multi_outputs,
                intra_scope_stores,
                terminal_place_id: None,
                terminal_type_cast_annotation: None,
                post_scope_destructure_idx: None,
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
            // Special case: if the scope output is a Destructure, the Destructure itself
            // doesn't produce a cacheable value. Instead, cache the Destructure's VALUE
            // and emit the Destructure post-scope using the scope output variable.
            if let InstructionValue::Destructure { value, .. } = &instr.value {
                let cache_expr = self.inlined_exprs.get(&value.identifier.0).cloned()
                    .unwrap_or_else(|| self.expr(value));
                return ScopeOutput {
                    outputs: vec![ScopeOutputItem {
                        skip_idx: Some(idx),
                        cache_expr,
                        out_name: None,
                        out_kw: "const",
                        is_named_var: false,
                        is_reassign: false,
                    }],
                    intra_scope_stores,
                    terminal_place_id: None,
                    terminal_type_cast_annotation: None,
                    post_scope_destructure_idx: Some(idx),
                };
            }
            // Prefer fresh try_inline_instr over stale build_inline_map entry.
            let mut cache_expr = if let Some(computed) = self.try_inline_instr(instr) {
                computed
            } else if let Some(inlined) = self.inlined_exprs.get(&instr.lvalue.identifier.0) {
                inlined.clone()
            } else {
                self.expr(&instr.lvalue)
            };
            // Method shorthand bodies (e.g. `() { return x; }`) start with `(` but
            // lack `function` keyword — prepend it for valid standalone emission.
            if let InstructionValue::FunctionExpression { fn_type: FunctionExpressionType::Expression, lowered_func, .. } = &instr.value {
                let async_kw = if lowered_func.func.async_ { "async " } else { "" };
                if cache_expr.starts_with('(') && !cache_expr.contains("=>") {
                    cache_expr = format!("{async_kw}function {cache_expr}");
                }
            }
            return ScopeOutput {
                outputs: vec![ScopeOutputItem {
                    skip_idx: Some(idx),
                    cache_expr,
                    out_name: None,
                    out_kw: "const",
                    is_named_var: false,
                        is_reassign: false,
                    }],
                intra_scope_stores,
                terminal_place_id: None,
                terminal_type_cast_annotation: None,
                post_scope_destructure_idx: None,
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
            // Special case: if the scope output is a Destructure, cache its VALUE
            // and emit the Destructure post-scope.
            if let InstructionValue::Destructure { value, .. } = &instr.value {
                let cache_expr = self.inlined_exprs.get(&value.identifier.0).cloned()
                    .unwrap_or_else(|| self.expr(value));
                return ScopeOutput {
                    outputs: vec![ScopeOutputItem {
                        skip_idx: Some(idx),
                        cache_expr,
                        out_name: None,
                        out_kw: "const",
                        is_named_var: false,
                        is_reassign: false,
                    }],
                    intra_scope_stores,
                    terminal_place_id: None,
                    terminal_type_cast_annotation: None,
                    post_scope_destructure_idx: Some(idx),
                };
            }
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
                        is_reassign: false,
                    }],
                intra_scope_stores,
                terminal_place_id: None,
                terminal_type_cast_annotation: None,
                post_scope_destructure_idx: None,
            };
        }

        ScopeOutput {
            outputs: vec![ScopeOutputItem {
                skip_idx: None,
                cache_expr: "undefined".to_string(),
                out_name: None,
                out_kw: "const",
                is_named_var: false,
                        is_reassign: false,
                    }],
            intra_scope_stores,
            terminal_place_id: None,
            terminal_type_cast_annotation: None,
            post_scope_destructure_idx: None,
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
                // Skip namespace alias assignments: `const localVar = NS` where
                // `localVar` is registered as a namespace alias. The alias is only
                // used to resolve JSX tags like `<localVar.Text>` → `<NS.Text>`.
                if let Some(ref n) = name {
                    if self.namespace_alias.contains_key(n.as_str()) {
                        return None;
                    }
                }
                let val_expr = self.expr(value);
                // Skip pre-scope declarations: Let-kind StoreLocal where the value has no
                // scope and the target variable is in a scope's reassignments.
                // e.g. `let s = null;` before a scope that reassigns `s = {}` inside.
                // The scope output mechanism emits `let s;` and `s = $[N];` instead.
                if matches!(lvalue.kind, InstructionKind::Let | InstructionKind::HoistedLet) {
                    let value_has_scope = self.env.get_identifier(value.identifier)
                        .and_then(|i| i.scope).is_some();
                    if std::env::var("RC_DEBUG").is_ok() {
                        eprintln!("[emit_stmt Let] name={:?} val_expr={:?} value_has_scope={} active_er={:?} scope_out_names={:?}",
                            name, val_expr, value_has_scope, self.active_early_return, scope_out_names);
                    }
                    if !value_has_scope && self.active_early_return.is_some() {
                        if let Some(ref n) = name {
                            if scope_out_names.contains(n) {
                                if std::env::var("RC_DEBUG").is_ok() {
                                    eprintln!("[emit_stmt Let] RETURNING NONE for {:?}", n);
                                }
                                return None;
                            }
                        }
                    }
                }
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
                        let uses = *self.use_count.get(&instr.lvalue.identifier.0).unwrap_or(&0);
                        let name = self.env.get_identifier(instr.lvalue.identifier)
                            .and_then(|i| i.name.as_ref())
                            .map(|n| n.value().to_string());
                        return if let Some(n) = name {
                            Some(format!("const {n} = {inner};"))
                        } else if uses == 0 {
                            // Result never used — emit inner as side-effect statement if it
                            // has side effects (contains a call), or skip entirely if pure.
                            if inner.contains('(') {
                                Some(format!("{inner};"))
                            } else {
                                None
                            }
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
                let c = self.expr(consequent);
                let c_wrapped = if c.contains(" ? ") { format!("({c})") } else { c };
                Some(format!("const {lv} = {} ? {c_wrapped} : {};", self.expr(test), self.expr(alternate)))
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
                // Always emit bracket notation for computed delete (matches TS compiler output).
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
                            } else if inner.contains('\\') {
                                // Any backslash escape (\\n, \\t, \\\\, etc.) would be
                                // misinterpreted in a JSX string attribute. Use JS expression.
                                format!("{name}={{{val}}}")
                            } else {
                                // Plain string without any backslashes: safe as bare string attr.
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
                    let mut src = apply_capture_renames(src, &lowered_func.func.context, self.env, &self.name_overrides);
                    // Apply namespace alias substitutions: replace `localVar.` with `NS.` in
                    // JSX tags and member access expressions within the function body.
                    for (alias_name, ns_name) in &self.namespace_alias {
                        // Replace `alias_name.` with `ns_name.` (word-boundary safe, only before '.')
                        src = rename_namespace_in_src(&src, alias_name, ns_name);
                    }
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
                                // Emit as hole if the identifier is unused.
                                // Check both direct use_count and declaration_id-based count
                                // to handle SSA phi chains (e.g., closures capturing loop vars).
                                let direct_cnt = *self.use_count.get(&p.identifier.0).unwrap_or(&0);
                                let decl_cnt = self.env.get_identifier(p.identifier)
                                    .and_then(|i| self.decl_id_use_count.get(&i.declaration_id.0))
                                    .copied()
                                    .unwrap_or(0);
                                if direct_cnt == 0 && decl_cnt == 0 {
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
                        // Skip empty array destructures (all holes/unused) — no bindings to extract.
                        if items.is_empty() && !kw.is_empty() {
                            return None;
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
                        if kw.is_empty() {
                            // Reassignment: wrap in parens to avoid ambiguity with block statement.
                            // `({ x, y } = val)` not `{ x, y } = val`
                            if props_str.is_empty() {
                                Some(format!("({{}} = {val});"))
                            } else {
                                Some(format!("({{ {props_str} }} = {val});"))
                            }
                        } else if props_str.is_empty() {
                            // Skip empty object destructures (const {} = val) — no bindings to extract.
                            None
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
        // scope_output_names takes priority over inlined_exprs:
        // if this identifier was assigned to a scope output (t0, t1, ...) during
        // scope emission, references to it outside the scope must use tN, not the
        // inlined expression (which would re-emit the full lambda/expression).
        if let Some(name) = self.scope_output_names.get(&place.identifier.0) {
            return name.clone();
        }
        if let Some(s) = self.inlined_exprs.get(&place.identifier.0) {
            return s.clone();
        }
        self.ident_name(place.identifier)
    }

    /// Collect update expressions for a for-loop update block.
    /// Walks the entire sub-CFG from `start` to just before `stop` (exclusive),
    /// collecting all non-inlined instructions and emitting them as expressions.
    /// Returns them as comma-separated strings (e.g. "x = x + t, x").
    fn collect_for_update_exprs(&mut self, start: crate::hir::hir::BlockId, stop: crate::hir::hir::BlockId) -> Vec<String> {
        use std::collections::{HashSet, VecDeque};

        // BFS to collect all blocks in update sub-CFG (stopping at `stop`).
        let mut visited_bfs: HashSet<crate::hir::hir::BlockId> = HashSet::new();
        let mut queue: VecDeque<crate::hir::hir::BlockId> = VecDeque::new();
        let mut blocks_in_order: Vec<crate::hir::hir::BlockId> = Vec::new();
        queue.push_back(start);
        while let Some(bid) = queue.pop_front() {
            if visited_bfs.contains(&bid) || bid == stop { continue; }
            visited_bfs.insert(bid);
            blocks_in_order.push(bid);
            let block = match self.hir.body.blocks.get(&bid) {
                Some(b) => b,
                None => continue,
            };
            for succ in block.terminal.successors() {
                if succ != stop && !visited_bfs.contains(&succ) {
                    queue.push_back(succ);
                }
            }
        }

        // Build a local inlining map for the update sub-CFG so that temps can
        // be resolved to their expressions without relying on the possibly-stale
        // global inlined_exprs map. Process blocks in order to inline in sequence.
        let mut local_exprs: std::collections::HashMap<u32, String> = std::collections::HashMap::new();

        // Helper: resolve a place's expression using local_exprs first, then global.
        // This avoids stale global entries for cross-block temps.
        let mut resolve = |place: &crate::hir::hir::Place,
                           local: &std::collections::HashMap<u32, String>,
                           global: &std::collections::HashMap<u32, String>,
                           ident_fn: &dyn Fn(crate::hir::hir::IdentifierId) -> String| -> String {
            if let Some(e) = local.get(&place.identifier.0) { return e.clone(); }
            if let Some(e) = global.get(&place.identifier.0) { return e.clone(); }
            ident_fn(place.identifier)
        };
        let _ = resolve; // used below via closure

        // Populate local_exprs by iterating all instructions in the sub-CFG.
        // For each block, compute inline expressions for all instructions.
        for bid in &blocks_in_order {
            let instrs: Vec<crate::hir::hir::Instruction> = self.hir.body.blocks.get(bid)
                .map(|b| b.instructions.clone())
                .unwrap_or_default();
            // Also handle phi nodes in this block (they represent join points like ternary results).
            let phis: Vec<crate::hir::hir::Phi> = self.hir.body.blocks.get(bid)
                .map(|b| b.phis.clone())
                .unwrap_or_default();
            // Phi results: use the global inlined_exprs (set by resolve_logical_phis).
            for phi in &phis {
                if let Some(e) = self.inlined_exprs.get(&phi.place.identifier.0) {
                    local_exprs.insert(phi.place.identifier.0, e.clone());
                }
                // Also: if the phi has a named user variable as an identifier, use that name.
                // This handles loop-carried phis where ident_name is a user variable.
                if !local_exprs.contains_key(&phi.place.identifier.0) {
                    let phi_name = self.ident_name(phi.place.identifier);
                    if !phi_name.starts_with("$t") && !phi_name.starts_with("$T") {
                        local_exprs.insert(phi.place.identifier.0, phi_name);
                    }
                }
            }
            for instr in &instrs {
                let id = instr.lvalue.identifier.0;
                // Compute fresh expression for this instruction.
                let expr = match &instr.value {
                    InstructionValue::Primitive { value, .. } => {
                        Some(primitive_expr(value))
                    }
                    InstructionValue::LoadLocal { place, .. } => {
                        // Resolve from local first (handles SSA temps), then global, then name.
                        let e = local_exprs.get(&place.identifier.0)
                            .or_else(|| self.inlined_exprs.get(&place.identifier.0))
                            .cloned()
                            .unwrap_or_else(|| self.ident_name(place.identifier));
                        Some(e)
                    }
                    InstructionValue::LoadGlobal { .. } => {
                        Some(self.ident_name(instr.lvalue.identifier))
                    }
                    InstructionValue::PropertyLoad { object, property, .. } => {
                        let obj = local_exprs.get(&object.identifier.0)
                            .or_else(|| self.inlined_exprs.get(&object.identifier.0))
                            .cloned()
                            .unwrap_or_else(|| self.ident_name(object.identifier));
                        Some(format!("{obj}.{property}"))
                    }
                    InstructionValue::BinaryExpression { operator, left, right, .. } => {
                        let op = binary_op_str(operator);
                        let l = local_exprs.get(&left.identifier.0)
                            .or_else(|| self.inlined_exprs.get(&left.identifier.0))
                            .cloned()
                            .unwrap_or_else(|| self.ident_name(left.identifier));
                        let r = local_exprs.get(&right.identifier.0)
                            .or_else(|| self.inlined_exprs.get(&right.identifier.0))
                            .cloned()
                            .unwrap_or_else(|| self.ident_name(right.identifier));
                        Some(format!("{l} {op} {r}"))
                    }
                    _ => None, // other instructions handled below
                };
                if let Some(e) = expr {
                    local_exprs.insert(id, e);
                }
            }
        }

        // Phase 3: Resolve ternary phi nodes.
        // For each block whose terminal is Branch(logical_op: None), the fallthrough
        // block has phi nodes representing the ternary result. Compute the ternary
        // expression from test/consequent/alternate and add to local_exprs.
        {
            use crate::hir::hir::Terminal as UpdTerm;
            for bid_idx in 0..blocks_in_order.len() {
                let bid = blocks_in_order[bid_idx];
                let (test_place, cons_bid, alt_bid, fall_bid) = {
                    let block = match self.hir.body.blocks.get(&bid) { Some(b) => b, None => continue };
                    match &block.terminal {
                        UpdTerm::Branch { test, consequent, alternate, fallthrough, logical_op: None, .. } => {
                            (test.clone(), *consequent, *alternate, *fallthrough)
                        }
                        _ => continue,
                    }
                };
                // Only handle if fallthrough is in our update sub-CFG region.
                if !visited_bfs.contains(&fall_bid) { continue; }
                let test_expr = local_exprs.get(&test_place.identifier.0)
                    .or_else(|| self.inlined_exprs.get(&test_place.identifier.0))
                    .cloned()
                    .unwrap_or_else(|| self.ident_name(test_place.identifier));
                let fall_phis: Vec<crate::hir::hir::Phi> = self.hir.body.blocks.get(&fall_bid)
                    .map(|b| b.phis.clone())
                    .unwrap_or_default();
                for phi in &fall_phis {
                    if local_exprs.contains_key(&phi.place.identifier.0) { continue; }
                    let mut cons_expr: Option<String> = None;
                    let mut alt_expr: Option<String> = None;
                    for (pred_bid, op_place) in &phi.operands {
                        let expr = local_exprs.get(&op_place.identifier.0)
                            .or_else(|| self.inlined_exprs.get(&op_place.identifier.0))
                            .cloned()
                            .unwrap_or_else(|| self.ident_name(op_place.identifier));
                        if *pred_bid == cons_bid {
                            cons_expr = Some(expr);
                        } else if *pred_bid == alt_bid {
                            alt_expr = Some(expr);
                        }
                    }
                    if let (Some(cv), Some(av)) = (cons_expr, alt_expr) {
                        local_exprs.insert(phi.place.identifier.0, format!("{test_expr} ? {cv} : {av}"));
                    }
                }
            }
        }

        // Phase 4: Resolve loop-carried phi nodes (multi-pass).
        // For phi nodes not yet resolved, try to find an operand that is already
        // in local_exprs, inlined_exprs, or has a user-visible name.
        for _ in 0..16 {
            let mut changed = false;
            for &bid in &blocks_in_order {
                let phis: Vec<crate::hir::hir::Phi> = self.hir.body.blocks.get(&bid)
                    .map(|b| b.phis.clone())
                    .unwrap_or_default();
                for phi in &phis {
                    if local_exprs.contains_key(&phi.place.identifier.0) { continue; }
                    // Try each operand.
                    for (_, op_place) in &phi.operands {
                        // Prefer already-resolved expressions.
                        if let Some(e) = local_exprs.get(&op_place.identifier.0) {
                            local_exprs.insert(phi.place.identifier.0, e.clone());
                            changed = true;
                            break;
                        }
                        if let Some(e) = self.inlined_exprs.get(&op_place.identifier.0) {
                            local_exprs.insert(phi.place.identifier.0, e.clone());
                            changed = true;
                            break;
                        }
                        // Use any named user variable among operands.
                        let name = self.ident_name(op_place.identifier);
                        if !name.starts_with("$t") && !name.starts_with("$T") {
                            local_exprs.insert(phi.place.identifier.0, name);
                            changed = true;
                            break;
                        }
                    }
                }
            }
            if !changed { break; }
        }

        // Phase 4b: Substitute newly-resolved phi expressions into existing local_exprs entries.
        // Phase 2 computed BinaryExpression etc. using ident_name("$t466") before phases 3/4
        // resolved those phis. Now update any stale "$tN" references using the resolved local_exprs.
        for _ in 0..8 {
            let mut changed = false;
            let keys: Vec<u32> = local_exprs.keys().copied().collect();
            for k in keys {
                let expr = local_exprs[&k].clone();
                if !expr.contains("$t") && !expr.contains("$T") { continue; }
                let new_expr = Self::substitute_temp_refs_in_str(&expr, &local_exprs);
                if new_expr != expr {
                    local_exprs.insert(k, new_expr);
                    changed = true;
                }
            }
            if !changed { break; }
        }

        // Build set of identifier IDs that are consumed as operands by instructions
        // in the update sub-CFG. Used to detect trailing LoadLocals.
        let mut used_in_update: std::collections::HashSet<u32> = std::collections::HashSet::new();
        for &bid in &blocks_in_order {
            let instrs = self.hir.body.blocks.get(&bid).map(|b| b.instructions.clone()).unwrap_or_default();
            for instr in &instrs {
                use crate::hir::visitors::each_instruction_value_operand;
                for place in each_instruction_value_operand(&instr.value) {
                    used_in_update.insert(place.identifier.0);
                }
            }
        }

        // Collect non-inlined side-effecting instructions (StoreLocals, calls, etc.)
        // from all blocks as update expressions.
        let mut exprs: Vec<String> = Vec::new();
        for bid in &blocks_in_order {
            let instrs: Vec<crate::hir::hir::Instruction> = self.hir.body.blocks.get(bid)
                .map(|b| b.instructions.clone())
                .unwrap_or_default();
            for instr in &instrs {
                // Skip instructions whose result is already inlined (consumed as a temp
                // by another instruction). Exclude LoadLocal so trailing reads can be emitted.
                let is_load_local = matches!(&instr.value, InstructionValue::LoadLocal { .. });
                if !is_load_local
                    && local_exprs.contains_key(&instr.lvalue.identifier.0)
                    && self.inlined_exprs.contains_key(&instr.lvalue.identifier.0)
                {
                    continue;
                }
                // Emit side-effectful instructions as expressions.
                match &instr.value {
                    InstructionValue::StoreLocal { lvalue, value, .. } => {
                        let lname = self.env.get_identifier(lvalue.place.identifier)
                            .and_then(|i| i.name.as_ref())
                            .map(|n| n.value().to_string());
                        if let Some(name) = lname {
                            let val_expr = local_exprs.get(&value.identifier.0)
                                .or_else(|| self.inlined_exprs.get(&value.identifier.0))
                                .cloned()
                                .unwrap_or_else(|| self.ident_name(value.identifier));
                            if name != val_expr {
                                exprs.push(format!("{name} = {val_expr}"));
                            }
                        }
                    }
                    InstructionValue::LoadLocal { place, .. } => {
                        // A naked LoadLocal in the update that is NOT consumed by another
                        // instruction in the update sub-CFG is a trailing "read" that should
                        // be emitted as part of the comma expression (e.g., `x = expr, x`).
                        if !used_in_update.contains(&instr.lvalue.identifier.0) {
                            let e = local_exprs.get(&place.identifier.0)
                                .or_else(|| self.inlined_exprs.get(&place.identifier.0))
                                .cloned()
                                .unwrap_or_else(|| self.ident_name(place.identifier));
                            exprs.push(e);
                        }
                    }
                    _ => {
                        // For other instructions not in local_exprs, try emit_stmt.
                        if !local_exprs.contains_key(&instr.lvalue.identifier.0) {
                            if let Some(s) = self.emit_stmt(instr, None, &[]) {
                                exprs.push(s.trim_end_matches(';').to_string());
                            }
                        }
                    }
                }
            }
        }
        exprs
    }

    /// Get the test expression for a do-while loop test block.
    /// When the test block has a logical_op (&&/||/??), the actual result is
    /// the phi in the fallthrough block — follow the chain until we find a
    /// Block without logical_op, then return that block's Branch.test.
    fn do_while_test_expr(&self, test_bid: crate::hir::hir::BlockId) -> String {
        use crate::hir::hir::Terminal;
        let mut current = test_bid;
        // Follow fallthrough through logical-op branches until we reach the
        // "real" branch (logical_op: None) whose test holds the phi result.
        for _ in 0..32 {
            let block = match self.hir.body.blocks.get(&current) {
                Some(b) => b,
                None => break,
            };
            match &block.terminal {
                Terminal::Branch { fallthrough, logical_op: Some(_), .. } => {
                    current = *fallthrough;
                }
                Terminal::Branch { test, .. } => {
                    return self.expr(test);
                }
                _ => break,
            }
        }
        // Fallback: use the original test block
        self.hir.body.blocks.get(&test_bid)
            .and_then(|b| if let Terminal::Branch { test, .. } = &b.terminal { Some(self.expr(test)) } else { None })
            .unwrap_or_else(|| "true".to_string())
    }

    /// Like `expr` but also tries to inline via `try_inline_instr`, ignoring
    /// use-count restrictions.  Used for phi operands in logical expressions
    /// where the same temp may appear multiple times (as Branch test + phi operand)
    /// but should still be inlined as a sub-expression.
    fn expr_for_phi_operand(&self, place: &Place) -> String {
        if let Some(s) = self.inlined_exprs.get(&place.identifier.0) {
            return s.clone();
        }
        if let Some(instr) = self.instr_map.get(&place.identifier.0) {
            if let Some(s) = self.try_inline_instr(instr) {
                return s;
            }
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
        // Check environment identifier name.
        // For Promoted $t/$T names, check promoted_temp_names first (rename_variables
        // equivalent for flat codegen). Named user vars are returned directly.
        let env_name = self.env
            .get_identifier(id)
            .and_then(|i| i.name.as_ref())
            .map(|n| n.value().to_string());
        if let Some(ref name) = env_name {
            use crate::hir::hir::IdentifierName;
            let is_promoted_temp = self.env
                .get_identifier(id)
                .and_then(|i| i.name.as_ref())
                .map(|n| matches!(n, IdentifierName::Promoted(_)) && (n.value().starts_with("$t") || n.value().starts_with("$T")))
                .unwrap_or(false);
            if is_promoted_temp {
                // Use promoted_temp_names if available (assigned by build_promoted_temp_names).
                if let Some(renamed) = self.promoted_temp_names.get(&id.0) {
                    return renamed.clone();
                }
                // Also check scope_output_names (scope emission may have already mapped this).
                if let Some(renamed) = self.scope_output_names.get(&id.0) {
                    return renamed.clone();
                }
                // Fall through to ssa_value_to_name etc.
                let _ = name; // suppress unused warning
            } else {
                return name.clone();
            }
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
        // Check promoted_temp_names (anonymous temps renamed to t0, t1, ... by build_promoted_temp_names).
        if let Some(name) = self.promoted_temp_names.get(&id.0) {
            return name.clone();
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
        // Check if this child is a nested JsxExpression or JsxFragment that will be
        // emitted inline (not cached in a scope variable). Only skip braces when the
        // actual emitted expression starts with `<` — i.e., it's inline JSX, not a
        // variable reference like `t0` that happens to hold JSX.
        let expr = self.expr(place);
        if expr.starts_with('<') {
            return expr;
        }
        // Expression child: wrap in {}.
        format!("{{{}}}", expr)
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

    /// Format a destructuring pattern (from a Destructure instruction's LValuePattern)
    /// as a string like `{a, b}` or `[x, y]`, suitable for use in `for (const PATTERN of ...)`.
    fn format_lvalue_pattern(&self, lvalue: &LValuePattern) -> String {
        match &lvalue.pattern {
            Pattern::Array(ap) => {
                let mut items: Vec<String> = ap.items.iter().map(|e| match e {
                    ArrayElement::Place(p) => {
                        if *self.use_count.get(&p.identifier.0).unwrap_or(&0) == 0 {
                            String::new()
                        } else {
                            self.ident_name(p.identifier)
                        }
                    }
                    ArrayElement::Spread(s) => format!("...{}", self.ident_name(s.place.identifier)),
                    ArrayElement::Hole => String::new(),
                }).collect();
                while items.last().map_or(false, |s| s.is_empty()) {
                    items.pop();
                }
                format!("[{}]", items.join(", "))
            }
            Pattern::Object(op) => {
                let props: Vec<String> = op.properties.iter().map(|p| match p {
                    ObjectPatternProperty::Property(prop) => {
                        let key_str = self.obj_key(prop.key.clone());
                        let ident_str = self.ident_name(prop.place.identifier);
                        let is_shorthand = matches!(prop.key, ObjectPropertyKey::Identifier(_))
                            && key_str == ident_str;
                        if is_shorthand { key_str } else { format!("{key_str}: {ident_str}") }
                    }
                    ObjectPatternProperty::Spread(s) => format!("...{}", self.ident_name(s.place.identifier)),
                }).collect();
                let props_str = props.join(", ");
                if props_str.is_empty() { "{}".to_string() } else { format!("{{ {} }}", props_str) }
            }
        }
    }

    /// Check if the loop body starts with a Destructure of the loop variable (identified by
    /// iter_next_id). If so, return the formatted pattern string and mark the Destructure as inlined.
    fn try_inline_for_of_destructure(
        &self,
        loop_bid: BlockId,
        iter_next_id: Option<u32>,
        loop_var_name: &str,
        inlined_ids: &mut std::collections::HashSet<u32>,
    ) -> String {
        if let Some(iter_id) = iter_next_id {
            if let Some(b) = self.hir.body.blocks.get(&loop_bid) {
                for instr in &b.instructions {
                    if let InstructionValue::Destructure { lvalue, value, .. } = &instr.value {
                        if value.identifier.0 == iter_id {
                            let pat = self.format_lvalue_pattern(lvalue);
                            inlined_ids.insert(instr.lvalue.identifier.0);
                            return pat;
                        }
                    }
                }
            }
        }
        loop_var_name.to_string()
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

    /// Compute the set of InstructionIds that live in catch-handler blocks.
    /// Instructions in catch handlers must NOT be assigned to any reactive scope,
    /// even if the scope inference tagged them (due to unioning through StoreLocal
    /// of variables that are also scope outputs in the try body).
    fn catch_handler_instruction_ids(&self) -> std::collections::HashSet<InstructionId> {
        use std::collections::{HashSet, VecDeque};

        // Collect all catch handler block IDs.
        let mut handler_blocks: HashSet<crate::hir::hir::BlockId> = HashSet::new();

        for block in self.hir.body.blocks.values() {
            if let Terminal::Try { handler, fallthrough, .. } = &block.terminal {
                // BFS from handler block up to (but not including) the fallthrough block.
                let fall_bid = *fallthrough;
                let mut queue = VecDeque::new();
                queue.push_back(*handler);
                while let Some(bid) = queue.pop_front() {
                    if bid == fall_bid { continue; }
                    if !handler_blocks.insert(bid) { continue; }
                    if let Some(b) = self.hir.body.blocks.get(&bid) {
                        for succ in b.terminal.successors() {
                            if succ != fall_bid && !handler_blocks.contains(&succ) {
                                queue.push_back(succ);
                            }
                        }
                    }
                }
            }
        }

        // Collect all InstructionIds in those blocks.
        let mut ids = HashSet::new();
        for bid in &handler_blocks {
            if let Some(block) = self.hir.body.blocks.get(bid) {
                for instr in &block.instructions {
                    ids.insert(instr.id);
                }
            }
        }
        ids
    }

    fn assign_instructions_to_scopes(
        &self,
        instrs: &[Instruction],
    ) -> HashMap<InstructionId, ScopeId> {
        use std::collections::HashSet;
        let mut map = HashMap::new();

        // Instructions in catch handler blocks must NOT be assigned to any scope.
        let catch_handler_ids = self.catch_handler_instruction_ids();

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

        // Build a local map from IdentifierId -> global/local name for callee resolution.
        // This is needed because inlined_exprs is populated during codegen (not yet available here).
        let mut callee_names: HashMap<u32, String> = HashMap::new();
        for instr in instrs {
            match &instr.value {
                InstructionValue::LoadGlobal { binding, .. } => {
                    callee_names.insert(instr.lvalue.identifier.0, self.binding_name(binding));
                }
                InstructionValue::LoadLocal { place, .. }
                | InstructionValue::LoadContext { place, .. } => {
                    let name = self.env.get_identifier(place.identifier)
                        .and_then(|i| i.name.as_ref())
                        .map(|n| n.value().to_string());
                    if let Some(n) = name {
                        callee_names.insert(instr.lvalue.identifier.0, n);
                    }
                }
                _ => {}
            }
        }

        for instr in instrs {
            // Instructions in catch handler blocks must never be placed in a scope.
            if catch_handler_ids.contains(&instr.id) {
                continue;
            }

            // Pre-check: hook calls and outlined FunctionExpressions must NEVER be placed
            // inside a memoization scope block — they must run unconditionally.
            // This check must happen before the ident.scope lookup (step 1) because
            // the scope inference may have tagged hook-call results as scope declarations,
            // but codegen must still emit them outside the scope block.
            let is_early_excluded = match &instr.value {
                InstructionValue::CallExpression { callee, .. } => {
                    callee_names.get(&callee.identifier.0)
                        .map(|name| is_react_hook_name(name))
                        .unwrap_or(false)
                }
                InstructionValue::FunctionExpression { name_hint, .. } => name_hint.is_some(),
                InstructionValue::GetIterator { .. }
                | InstructionValue::IteratorNext { .. }
                | InstructionValue::NextPropertyOf { .. } => true,
                _ => false,
            };
            if is_early_excluded {
                continue;
            }

            // Detect `let s = null;` pattern: StoreLocal with Let/HoistedLet kind where
            // the value has no scope AND the target variable is in a scope's reassignments.
            // This represents a pre-scope declaration (e.g., `let s = null;` before a scope
            // that later reassigns `s = {}`). Such instructions must NOT be tagged to any
            // scope — the scope output mechanism handles `let s;` via its reassignment output.
            // NOTE: Only applies when the place is in reassignments, NOT declarations.
            // Scope output declarations (e.g., `let t0 = callExpr();`) must still be tagged.
            let is_let_decl_with_unscoped_value = match &instr.value {
                InstructionValue::StoreLocal { lvalue, value, .. }
                    if matches!(lvalue.kind, InstructionKind::Let | InstructionKind::HoistedLet) =>
                {
                    let value_unscoped = self.env.get_identifier(value.identifier)
                        .and_then(|i| i.scope).is_none();
                    if !value_unscoped { false } else {
                        // Only treat as pre-scope declaration if the place is in some
                        // scope's reassignments (not declarations). Declarations need to be
                        // tagged to their scope so they're emitted inside the scope body.
                        let place_id = lvalue.place.identifier;
                        self.env.scopes.values().any(|scope| scope.reassignments.contains(&place_id))
                    }
                }
                _ => false,
            };

            // 1. Check the instruction's own lvalue identifier.
            if !is_let_decl_with_unscoped_value {
                if let Some(ident) = self.env.get_identifier(instr.lvalue.identifier) {
                    if let Some(sid) = ident.scope {
                        map.insert(instr.id, sid);
                        continue;
                    }
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
                        let value_scope = self.env.get_identifier(value.identifier).and_then(|i| i.scope);
                        if value_scope.is_some() {
                            value_scope
                        } else if matches!(lvalue.kind, InstructionKind::Let | InstructionKind::HoistedLet) {
                            // For Let-kind declarations with unscoped values, don't fall
                            // back to the place's scope — this is a pre-scope declaration.
                            None
                        } else {
                            self.env.get_identifier(lvalue.place.identifier).and_then(|i| i.scope)
                        }
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
                // Mutations of scope-owned objects: assign to the object's scope.
                // This ensures `delete x["b"]`, `x.foo = bar`, etc. are emitted
                // inside the scope block that owns `x`, not outside it.
                // The object may be a LoadLocal result; we trace through it to
                // find the underlying variable's scope.
                InstructionValue::PropertyDelete { object, .. }
                | InstructionValue::ComputedDelete { object, .. }
                | InstructionValue::PropertyStore { object, .. }
                | InstructionValue::ComputedStore { object, .. } => {
                    // First check the object identifier directly.
                    let direct = self.env.get_identifier(object.identifier).and_then(|i| i.scope);
                    if direct.is_some() {
                        direct
                    } else {
                        // Trace through LoadLocal: find the instruction that produced `object`.
                        if let Some(obj_instr) = self.instr_map.get(&object.identifier.0) {
                            if let InstructionValue::LoadLocal { place, .. }
                            | InstructionValue::LoadContext { place, .. } = &obj_instr.value {
                                self.env.get_identifier(place.identifier).and_then(|i| i.scope)
                            } else {
                                None
                            }
                        } else {
                            None
                        }
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
                    callee_names.get(&callee.identifier.0)
                        .map(|name| is_react_hook_name(name))
                        .unwrap_or(false)
                }
                InstructionValue::FunctionExpression { name_hint, .. } => name_hint.is_some(),
                // Also exclude StoreLocal/StoreContext of outlined function results
                // even when they fall through to the range-based assignment (step 3).
                InstructionValue::StoreLocal { value, .. } => outlined_fn_ids.contains(&value.identifier.0),
                InstructionValue::StoreContext { value, .. } => outlined_fn_ids.contains(&value.identifier.0),
                // For-of/for-in loop infrastructure instructions must never be placed
                // inside a memoization scope block — they are structural parts of the
                // loop control flow and are inlined into the loop header by the codegen.
                InstructionValue::GetIterator { .. }
                | InstructionValue::IteratorNext { .. }
                | InstructionValue::NextPropertyOf { .. } => true,
                // Destructure instructions that source from a parameter (mutable_range.start == 0)
                // must be emitted outside scope blocks so extracted values are available as deps.
                InstructionValue::Destructure { value, .. } => {
                    self.env.get_identifier(value.identifier)
                        .map(|i| i.mutable_range.start.0 == 0)
                        .unwrap_or(false)
                }
                _ => false,
            };
            if !is_excluded && !is_let_decl_with_unscoped_value {
                for (sid, scope) in &self.env.scopes {
                    let range_nonempty = scope.range.end > scope.range.start;
                    if range_nonempty && instr.id >= scope.range.start && instr.id < scope.range.end {
                        map.entry(instr.id).or_insert(*sid);
                    }
                }
            }
        }

        // (No post-processing pruning needed — scope assignments are final.)

        map
    }

    // -----------------------------------------------------------------------
    // Tree-based (ReactiveBlock) emission
    // -----------------------------------------------------------------------

    /// Walk a ReactiveBlock tree and emit JS statements to `out`.
    /// `scope_out_names` is the set of variable names that are scope outputs
    /// (should be assigned without `const`/`let` declaration prefix).
    /// `indent` is the indentation level (1 = 2 spaces per level).
    #[allow(clippy::too_many_arguments)]
    fn codegen_tree_block(
        &mut self,
        block: &[crate::hir::hir::ReactiveStatement],
        scope_out_names: &[String],
        indent: usize,
        out: &mut String,
        scope_instrs: &std::collections::HashMap<ScopeId, Vec<Instruction>>,
        inlined_ids: &std::collections::HashSet<u32>,
        scope_index: &mut usize,
        declared_names: &mut std::collections::HashSet<String>,
    ) {
        use crate::hir::hir::{ReactiveStatement, ReactiveValue};

        let pad = "  ".repeat(indent);

        // Track instruction IDs consumed by scope emission so they aren't re-emitted
        // as siblings. The tree builder places some scope output instructions (StoreLocal)
        // outside the Scope node, but emit_scope_block_inner already handles them via skip_idx.
        let mut consumed_instr_ids: std::collections::HashSet<u32> = std::collections::HashSet::new();

        // Pre-compute for-init instruction IDs: if this block contains a Terminal(For),
        // collect the trailing DeclareLocal/StoreLocal instruction IDs from init_bid so they
        // are suppressed from regular statement emission (they'll be emitted in the for-init).
        let for_init_ids: std::collections::HashSet<u32> = {
            let mut ids = std::collections::HashSet::new();
            for stmt in block {
                if let ReactiveStatement::Terminal(term_stmt) = stmt {
                    if let crate::hir::hir::ReactiveTerminal::For { init_bid, .. } = &term_stmt.terminal {
                        if let Some(init_block) = self.hir.body.blocks.get(init_bid) {
                            for instr in init_block.instructions.iter().rev() {
                                match &instr.value {
                                    InstructionValue::DeclareLocal { .. } | InstructionValue::StoreLocal { .. } => {
                                        ids.insert(instr.lvalue.identifier.0);
                                    }
                                    _ => break,
                                }
                            }
                        }
                    }
                }
            }
            ids
        };

        for stmt in block {
            match stmt {
                ReactiveStatement::Instruction(reactive_instr) => {
                    // Skip instructions already consumed by a scope in this block.
                    let lv_id = reactive_instr.lvalue.as_ref().map(|p| p.identifier.0).unwrap_or(u32::MAX);
                    if consumed_instr_ids.contains(&lv_id) {
                        continue;
                    }
                    // Handle synthetic instructions with no flat-HIR lvalue.
                    // These are created by propagate_early_returns: StoreLocal(sentinel, val).
                    // Emit as `sentinel_var = val_expr;` using scope_output_names for the name.
                    if lv_id == u32::MAX {
                        if let ReactiveValue::Instruction(InstructionValue::StoreLocal { lvalue: inner_lv, value, .. }) = &reactive_instr.value {
                            let inner_id = inner_lv.place.identifier;
                            let target_name = self.scope_output_names.get(&inner_id.0)
                                .cloned()
                                .unwrap_or_else(|| self.ident_name(inner_id));
                            let val_expr = self.expr(value);
                            let _ = std::fmt::write(out, format_args!("{pad}{target_name} = {val_expr};\n"));
                        }
                        continue;
                    }
                    // Skip instructions that are inlined at use sites (no standalone emission needed).
                    if inlined_ids.contains(&lv_id) {
                        continue;
                    }
                    // Skip for-init instructions — they will be emitted in the for(...) header.
                    if for_init_ids.contains(&lv_id) {
                        continue;
                    }
                    // Skip promoted-temp lvalues (names starting with "$t" or "$T") —
                    // these are SSA temporaries always inlined at use sites, not standalone stmts.
                    let lv_name_opt = reactive_instr.lvalue.as_ref()
                        .and_then(|p| self.env.get_identifier(p.identifier))
                        .and_then(|i| i.name.as_ref())
                        .map(|n| n.value().to_string());
                    if lv_name_opt.as_deref().map(|n| n.starts_with("$t") || n.starts_with("$T")).unwrap_or(false) {
                        continue;
                    }
                    // Track DeclareLocal names for double-declaration prevention.
                    if let ReactiveValue::Instruction(InstructionValue::DeclareLocal { lvalue, .. }) = &reactive_instr.value {
                        if let Some(name) = self.env.get_identifier(lvalue.place.identifier)
                            .and_then(|i| i.name.as_ref())
                            .map(|n| n.value().to_string())
                        {
                            declared_names.insert(name);
                        }
                    }
                    // Look up the flat HIR instruction by its lvalue identifier.
                    let flat_instr = self.instr_map.get(&lv_id).cloned();
                    if let Some(instr) = flat_instr {
                        if let Some(s) = self.emit_stmt(&instr.clone(), None, scope_out_names) {
                            for line in s.lines() {
                                let _ = std::fmt::write(out, format_args!("{pad}{}\n", line));
                            }
                        }
                    }
                }
                ReactiveStatement::PrunedScope(pruned) => {
                    // Pruned scopes don't emit cache logic — just their instructions.
                    self.codegen_tree_block(&pruned.instructions, scope_out_names, indent, out, scope_instrs, inlined_ids, scope_index, declared_names);
                }
                ReactiveStatement::Scope(scope_block) => {
                    let sid = scope_block.scope.id;
                    let instrs_list = scope_instrs.get(&sid).cloned().unwrap_or_default();
                    // Mark all flat instructions consumed by this scope so siblings don't re-emit them.
                    for i in &instrs_list {
                        consumed_instr_ids.insert(i.lvalue.identifier.0);
                    }
                    let instr_refs: Vec<&Instruction> = instrs_list.iter().collect();
                    let tree_body = Some((scope_block.instructions.as_slice(), scope_instrs));
                    self.emit_scope_block_inner(
                        &sid,
                        &instr_refs,
                        indent,
                        scope_index,
                        out,
                        inlined_ids,
                        declared_names,
                        tree_body,
                    );
                }
                ReactiveStatement::Terminal(term_stmt) => {
                    // For Switch terminals with a label (bb0: switch ...), emit labeled switch.
                    // For Label terminals wrapping a labeled block, prepend label to first line.
                    let terminal_label_opt: Option<String> = term_stmt.label.as_ref()
                        .and_then(|l| self.switch_fallthrough_labels.get(&l.id).cloned())
                        .filter(|_| matches!(&term_stmt.terminal,
                            crate::hir::hir::ReactiveTerminal::Switch { .. }
                            | crate::hir::hir::ReactiveTerminal::Label { .. }
                        ));
                    if let Some(label) = terminal_label_opt {
                        if let crate::hir::hir::ReactiveTerminal::Label { block, .. } = &term_stmt.terminal {
                            let is_early_return_label = term_stmt.label.as_ref()
                                .map(|l| self.early_return_label_blocks.contains(&l.id))
                                .unwrap_or(false);
                            if is_early_return_label {
                                // Early-return label: wrap body in braces so all stmts are labeled.
                                // Emits: `bb0: {\n  body...\n}`
                                let mut temp = String::new();
                                self.codegen_tree_block(block, scope_out_names, indent + 1, &mut temp, scope_instrs, inlined_ids, scope_index, declared_names);
                                let _ = std::fmt::write(out, format_args!("{pad}{label}: {{\n"));
                                out.push_str(&temp);
                                let _ = std::fmt::write(out, format_args!("{pad}}}\n"));
                            } else {
                                // Regular label: prepend label to the first line.
                                let mut temp = String::new();
                                self.codegen_tree_block(block, scope_out_names, indent, &mut temp, scope_instrs, inlined_ids, scope_index, declared_names);
                                let labeled = temp.replacen(&pad, &format!("{pad}{label}: "), 1);
                                out.push_str(&labeled);
                            }
                        } else {
                            // Switch terminal: emit the switch, prepend label to the switch line.
                            let mut temp = String::new();
                            self.codegen_tree_terminal(&term_stmt.terminal, scope_out_names, indent, &mut temp, scope_instrs, inlined_ids, scope_index, declared_names);
                            let labeled = temp.replacen(&pad, &format!("{pad}{label}: "), 1);
                            out.push_str(&labeled);
                        }
                    } else {
                        self.codegen_tree_terminal(&term_stmt.terminal, scope_out_names, indent, out, scope_instrs, inlined_ids, scope_index, declared_names);
                    }
                }
            }
        }
    }

    /// Emit a reactive scope block with memoization if/else.
    #[allow(clippy::too_many_arguments)]
    fn codegen_tree_scope(
        &mut self,
        scope_block: &crate::hir::hir::ReactiveScopeBlock,
        indent: usize,
        out: &mut String,
        scope_instrs: &std::collections::HashMap<ScopeId, Vec<Instruction>>,
        inlined_ids: &std::collections::HashSet<u32>,
        scope_index: &mut usize,
        declared_names: &mut std::collections::HashSet<String>,
    ) {
        let pad = "  ".repeat(indent);
        let body_pad = "  ".repeat(indent + 1);
        let scope = &scope_block.scope;
        let scope_id = scope.id;

        // Collect declaration variable names (sorted by id for determinism).
        let mut decl_ids: Vec<crate::hir::hir::IdentifierId> = scope.declarations.keys().copied().collect();
        decl_ids.sort_by_key(|id| id.0);

        let mut scope_out_names: Vec<String> = Vec::new();
        for id in &decl_ids {
            let name = self.ident_name(*id);
            if !name.starts_with("$t") {
                scope_out_names.push(name.clone());
                // Skip `let name;` if already declared by outer DeclareLocal.
                if !declared_names.contains(&name) {
                    let _ = std::fmt::write(out, format_args!("{pad}let {name};\n"));
                }
            }
        }

        // Also collect reassignment names.
        let mut reassign_ids = scope.reassignments.clone();
        reassign_ids.sort_by_key(|id| id.0);
        for id in &reassign_ids {
            let name = self.ident_name(*id);
            if !name.starts_with("$t") && !scope_out_names.contains(&name) {
                scope_out_names.push(name.clone());
                // Skip `let name;` if already declared by outer DeclareLocal.
                if !declared_names.contains(&name) {
                    let _ = std::fmt::write(out, format_args!("{pad}let {name};\n"));
                }
            }
        }

        // Get pre-assigned cache slots from the existing slot map.
        let dep_slots = self.dep_slots.get(&scope_id).cloned().unwrap_or_default();
        let out_slots = self.output_slots.get(&scope_id).cloned().unwrap_or_default();

        // Build dependency expressions.
        let deps: Vec<_> = scope.dependencies.iter().cloned().collect();
        let dep_exprs: Vec<String> = deps.iter().map(|d| self.dep_expr(d)).collect();

        // Build the test condition.
        let has_deps = !dep_slots.is_empty() && !dep_exprs.is_empty();
        let condition = if has_deps {
            let parts: Vec<String> = dep_slots.iter().zip(dep_exprs.iter())
                .map(|(&slot, dep_expr)| format!("$[{slot}] !== {dep_expr}"))
                .collect();
            parts.join(" || ")
        } else if !out_slots.is_empty() {
            // No deps: invalidate based on sentinel.
            format!("$[{}] === Symbol.for(\"react.memo_cache_sentinel\")", out_slots[0])
        } else {
            // No slots at all — no memoization condition; just emit body.
            self.codegen_tree_block(&scope_block.instructions, &scope_out_names, indent, out, scope_instrs, inlined_ids, scope_index, declared_names);
            return;
        };

        // Emit: if (condition) { body; cache stores; } else { cache loads; }
        let _ = std::fmt::write(out, format_args!("{pad}if ({condition}) {{\n"));

        // Emit scope body.
        let mut body_str = String::new();
        self.codegen_tree_block(&scope_block.instructions, &scope_out_names, indent + 1, &mut body_str, scope_instrs, inlined_ids, scope_index, declared_names);
        out.push_str(&body_str);

        // Emit cache stores for deps.
        for (&slot, dep_expr) in dep_slots.iter().zip(dep_exprs.iter()) {
            let _ = std::fmt::write(out, format_args!("{body_pad}$[{slot}] = {dep_expr};\n"));
        }
        // Emit cache stores for outputs.
        for (&slot, name) in out_slots.iter().zip(scope_out_names.iter()) {
            let _ = std::fmt::write(out, format_args!("{body_pad}$[{slot}] = {name};\n"));
        }

        let _ = std::fmt::write(out, format_args!("{pad}}} else {{\n"));

        // Emit cache loads.
        for (&slot, name) in out_slots.iter().zip(scope_out_names.iter()) {
            let _ = std::fmt::write(out, format_args!("{body_pad}{name} = $[{slot}];\n"));
        }

        let _ = std::fmt::write(out, format_args!("{pad}}}\n"));
    }

    /// Emit a reactive terminal (if, switch, loops, return, etc.).
    #[allow(clippy::too_many_arguments)]
    fn codegen_tree_terminal(
        &mut self,
        terminal: &crate::hir::hir::ReactiveTerminal,
        scope_out_names: &[String],
        indent: usize,
        out: &mut String,
        scope_instrs: &std::collections::HashMap<ScopeId, Vec<Instruction>>,
        inlined_ids: &std::collections::HashSet<u32>,
        scope_index: &mut usize,
        declared_names: &mut std::collections::HashSet<String>,
    ) {
        use crate::hir::hir::ReactiveTerminal;
        let pad = "  ".repeat(indent);

        match terminal {
            ReactiveTerminal::Return { value, .. } => {
                let expr = if let Some(replacement) = self.terminal_replacement.get(&value.identifier.0) {
                    replacement.clone()
                } else {
                    self.expr(value)
                };
                if expr == "undefined" {
                    // Suppress `return undefined;` at function body level (indent==1) — it's the
                    // implicit function end. Inside a nested block (indent>1) emit bare `return;`
                    // so early exits are preserved (e.g., `if (cond) { return; }`).
                    if indent > 1 {
                        let _ = std::fmt::write(out, format_args!("{pad}return;\n"));
                    }
                } else {
                    let _ = std::fmt::write(out, format_args!("{pad}return {expr};\n"));
                }
            }
            ReactiveTerminal::Throw { value, .. } => {
                let expr = self.expr(value);
                let _ = std::fmt::write(out, format_args!("{pad}throw {expr};\n"));
            }
            ReactiveTerminal::Break { target, .. } => {
                // target is the fallthrough BlockId; if it's in switch_fallthrough_labels,
                // emit a labeled break (break bb0;) regardless of target_kind.
                if let Some(label) = self.switch_fallthrough_labels.get(target).cloned() {
                    let _ = std::fmt::write(out, format_args!("{pad}break {label};\n"));
                } else {
                    let _ = std::fmt::write(out, format_args!("{pad}break;\n"));
                }
            }
            ReactiveTerminal::Continue { .. } => {
                let _ = std::fmt::write(out, format_args!("{pad}continue;\n"));
            }
            ReactiveTerminal::If { test, consequent, alternate, .. } => {
                let test_expr = self.expr(test);
                let _ = std::fmt::write(out, format_args!("{pad}if ({test_expr}) {{\n"));
                self.codegen_tree_block(consequent, scope_out_names, indent + 1, out, scope_instrs, inlined_ids, scope_index, declared_names);
                if let Some(alt) = alternate {
                    if !alt.is_empty() {
                        let _ = std::fmt::write(out, format_args!("{pad}}} else {{\n"));
                        self.codegen_tree_block(alt, scope_out_names, indent + 1, out, scope_instrs, inlined_ids, scope_index, declared_names);
                    }
                }
                let _ = std::fmt::write(out, format_args!("{pad}}}\n"));
            }
            ReactiveTerminal::While { test, loop_, test_bid, .. } => {
                // Use do_while_test_expr to correctly resolve compound tests (e.g. x && y).
                let test_expr = {
                    let expr = self.do_while_test_expr(*test_bid);
                    if expr == "true" || expr.is_empty() {
                        self.reactive_value_expr(test) // fallback
                    } else {
                        expr
                    }
                };
                let _ = std::fmt::write(out, format_args!("{pad}while ({test_expr}) {{\n"));
                let mut loop_out = String::new();
                self.codegen_tree_block(loop_, scope_out_names, indent + 1, &mut loop_out, scope_instrs, inlined_ids, scope_index, declared_names);
                out.push_str(&strip_trailing_continue(&loop_out, indent + 1));
                let _ = std::fmt::write(out, format_args!("{pad}}}\n"));
            }
            ReactiveTerminal::DoWhile { loop_, test, test_bid, .. } => {
                let _ = std::fmt::write(out, format_args!("{pad}do {{\n"));
                let mut loop_out = String::new();
                self.codegen_tree_block(loop_, scope_out_names, indent + 1, &mut loop_out, scope_instrs, inlined_ids, scope_index, declared_names);
                out.push_str(&strip_trailing_continue(&loop_out, indent + 1));
                // Use do_while_test_expr to correctly resolve compound tests (e.g. x && y).
                let test_expr = {
                    let expr = self.do_while_test_expr(*test_bid);
                    if expr == "true" || expr.is_empty() {
                        self.reactive_value_expr(test) // fallback for simple tests
                    } else {
                        expr
                    }
                };
                let _ = std::fmt::write(out, format_args!("{pad}}} while ({test_expr});\n"));
            }
            ReactiveTerminal::For { loop_, init_bid, test_bid, update_bid, .. } => {
                // Collect trailing DeclareLocal/StoreLocal from the init block as the for-init
                // expression (same approach as flat codegen). These instructions were suppressed
                // from regular emission in codegen_tree_block via for_init_ids.
                let init_expr = self.hir.body.blocks.get(init_bid).map(|b| {
                    // Walk backwards from the end of init_bid to collect trailing
                    // DeclareLocal/StoreLocal instructions (same as flat codegen marking).
                    let mut rev_parts: Vec<String> = Vec::new();
                    for instr in b.instructions.iter().rev() {
                        match &instr.value {
                            InstructionValue::DeclareLocal { .. } | InstructionValue::StoreLocal { .. } => {
                                if let Some(s) = self.emit_stmt(instr, None, scope_out_names) {
                                    rev_parts.push(s.trim_end_matches(';').to_string());
                                }
                            }
                            _ => break,
                        }
                    }
                    rev_parts.reverse();
                    rev_parts.join(", ")
                }).unwrap_or_default();
                let test_expr = self.hir.body.blocks.get(test_bid).and_then(|b| {
                    if let crate::hir::hir::Terminal::Branch { test, .. } = &b.terminal {
                        Some(self.expr(test))
                    } else { None }
                }).unwrap_or_else(|| "true".to_string());
                let update_expr = update_bid.and_then(|ubid| {
                    self.hir.body.blocks.get(&ubid).and_then(|b| {
                        let mut last = None;
                        for instr in &b.instructions {
                            match &instr.value {
                                InstructionValue::Primitive { value, .. } => { last = Some(primitive_expr(value)); }
                                InstructionValue::LoadLocal { place, .. } => { last = Some(self.expr(place)); }
                                _ => {
                                    if let Some(s) = self.emit_stmt(instr, None, scope_out_names) {
                                        last = Some(s.trim_end_matches(';').to_string());
                                    }
                                }
                            }
                        }
                        last
                    })
                }).unwrap_or_default();
                let _ = std::fmt::write(out, format_args!("{pad}for ({init_expr}; {test_expr}; {update_expr}) {{\n"));
                let mut loop_out = String::new();
                self.codegen_tree_block(loop_, scope_out_names, indent + 1, &mut loop_out, scope_instrs, inlined_ids, scope_index, declared_names);
                out.push_str(&strip_trailing_continue(&loop_out, indent + 1));
                let _ = std::fmt::write(out, format_args!("{pad}}}\n"));
            }
            ReactiveTerminal::ForOf { loop_var, iterable, loop_, iterable_bid, loop_bid, .. } => {
                // Use flat-codegen local_exprs approach to resolve iterable (handles named
                // promoted temps like $t21 = PropertyLoad that aren't in inlined_exprs).
                let iterable_expr = self.forof_init_expr(*iterable_bid)
                    .unwrap_or_else(|| self.reactive_value_expr(iterable));
                // Detect destructuring pattern in loop body (e.g. `const [, entry] = item`).
                // Use a local clone so we can add the Destructure's lvalue to inlined_ids for
                // the loop body without affecting sibling instructions.
                let iter_next_id = self.hir.body.blocks.get(loop_bid).and_then(|b| {
                    b.instructions.iter().find_map(|instr| {
                        match &instr.value {
                            InstructionValue::StoreLocal { value, .. } => Some(value.identifier.0),
                            InstructionValue::Destructure { value, .. } => Some(value.identifier.0),
                            _ => None,
                        }
                    })
                });
                let mut loop_inlined = inlined_ids.clone();
                let for_of_pattern = self.try_inline_for_of_destructure(*loop_bid, iter_next_id, loop_var, &mut loop_inlined);
                let _ = std::fmt::write(out, format_args!("{pad}for (const {for_of_pattern} of {iterable_expr}) {{\n"));
                let mut loop_out = String::new();
                self.codegen_tree_block(loop_, scope_out_names, indent + 1, &mut loop_out, scope_instrs, &loop_inlined, scope_index, declared_names);
                out.push_str(&strip_trailing_continue(&loop_out, indent + 1));
                let _ = std::fmt::write(out, format_args!("{pad}}}\n"));
            }
            ReactiveTerminal::ForIn { loop_var, object, loop_, object_bid, .. } => {
                // Use flat-codegen local_exprs approach for ForIn object.
                let object_expr = self.forof_init_expr(*object_bid)
                    .unwrap_or_else(|| self.reactive_value_expr(object));
                let _ = std::fmt::write(out, format_args!("{pad}for (const {loop_var} in {object_expr}) {{\n"));
                let mut loop_out = String::new();
                self.codegen_tree_block(loop_, scope_out_names, indent + 1, &mut loop_out, scope_instrs, inlined_ids, scope_index, declared_names);
                out.push_str(&strip_trailing_continue(&loop_out, indent + 1));
                let _ = std::fmt::write(out, format_args!("{pad}}}\n"));
            }
            ReactiveTerminal::Switch { test, cases, .. } => {
                let test_expr = self.expr(test);
                let _ = std::fmt::write(out, format_args!("{pad}switch ({test_expr}) {{\n"));
                for case in cases {
                    if let Some(case_test) = &case.test {
                        let case_expr = self.expr(case_test);
                        let _ = std::fmt::write(out, format_args!("{pad}  case {case_expr}: {{\n"));
                    } else {
                        let _ = std::fmt::write(out, format_args!("{pad}  default: {{\n"));
                    }
                    if let Some(block) = &case.block {
                        self.codegen_tree_block(block, scope_out_names, indent + 2, out, scope_instrs, inlined_ids, scope_index, declared_names);
                    }
                    let _ = std::fmt::write(out, format_args!("{pad}  }}\n"));
                }
                let _ = std::fmt::write(out, format_args!("{pad}}}\n"));
            }
            ReactiveTerminal::Label { block, .. } => {
                self.codegen_tree_block(block, scope_out_names, indent, out, scope_instrs, inlined_ids, scope_index, declared_names);
            }
            ReactiveTerminal::Try { block, handler_binding, handler, .. } => {
                let _ = std::fmt::write(out, format_args!("{pad}try {{\n"));
                self.codegen_tree_block(block, scope_out_names, indent + 1, out, scope_instrs, inlined_ids, scope_index, declared_names);
                if let Some(binding) = handler_binding {
                    let binding_name = self.ident_name(binding.identifier);
                    let _ = std::fmt::write(out, format_args!("{pad}}} catch ({binding_name}) {{\n"));
                } else {
                    let _ = std::fmt::write(out, format_args!("{pad}}} catch {{\n"));
                }
                self.codegen_tree_block(handler, scope_out_names, indent + 1, out, scope_instrs, inlined_ids, scope_index, declared_names);
                let _ = std::fmt::write(out, format_args!("{pad}}}\n"));
            }
        }
    }

    /// Emit a ReactiveValue as an expression string (for loop conditions etc.).
    /// Build the iterable/object expression for a ForOf/ForIn from its init block.
    /// Uses the same local_exprs chain as the flat codegen, correctly resolving
    /// named promoted temps (e.g. $t21 = PropertyLoad) that aren't in inlined_exprs.
    fn forof_init_expr(&self, bid: crate::hir::hir::BlockId) -> Option<String> {
        let block = self.hir.body.blocks.get(&bid)?;
        let mut local_exprs: std::collections::HashMap<u32, String> = std::collections::HashMap::new();
        let mut result: Option<String> = None;
        for instr in &block.instructions {
            match &instr.value {
                InstructionValue::LoadLocal { place, .. }
                | InstructionValue::LoadContext { place, .. } => {
                    let name = self.expr(place);
                    local_exprs.insert(instr.lvalue.identifier.0, name);
                }
                InstructionValue::LoadGlobal { binding, .. } => {
                    let name = self.binding_name(binding);
                    local_exprs.insert(instr.lvalue.identifier.0, name);
                }
                InstructionValue::PropertyLoad { object, property, .. } => {
                    let obj = local_exprs.get(&object.identifier.0)
                        .cloned()
                        .unwrap_or_else(|| self.expr(object));
                    local_exprs.insert(instr.lvalue.identifier.0, format!("{obj}.{property}"));
                }
                InstructionValue::MethodCall { receiver, property, args, .. } => {
                    // Handle method calls like `mapping.values()` or `s1.values()`.
                    let recv = local_exprs.get(&receiver.identifier.0)
                        .cloned()
                        .unwrap_or_else(|| self.expr(receiver));
                    let method_suffix = self.method_suffix_from_place(property);
                    let call_args = self.call_args(args);
                    local_exprs.insert(instr.lvalue.identifier.0, format!("{recv}{method_suffix}({call_args})"));
                }
                InstructionValue::CallExpression { callee, args, .. } => {
                    // Handle plain call expressions like `someFunc()`.
                    let callee_expr = local_exprs.get(&callee.identifier.0)
                        .cloned()
                        .unwrap_or_else(|| self.expr(callee));
                    let call_args = self.call_args(args);
                    local_exprs.insert(instr.lvalue.identifier.0, format!("{callee_expr}({call_args})"));
                }
                InstructionValue::GetIterator { collection, .. }
                | InstructionValue::NextPropertyOf { value: collection, .. } => {
                    // The collection IS the iterable expression.
                    result = Some(local_exprs.get(&collection.identifier.0)
                        .cloned()
                        .unwrap_or_else(|| self.expr(collection)));
                }
                _ => {}
            }
        }
        result
    }

    fn reactive_value_expr(&self, value: &crate::hir::hir::ReactiveValue) -> String {
        use crate::hir::hir::ReactiveValue;
        match value {
            ReactiveValue::Instruction(instr_val) => {
                // Resolve instruction values to expressions. The primary cases are LoadLocal
                // (used for loop test blocks) and PropertyLoad (e.g., `props.cond`).
                // All temp lvalues should be resolvable via inlined_exprs.
                match instr_val {
                    InstructionValue::LoadLocal { place, .. } |
                    InstructionValue::LoadContext { place, .. } => self.expr(place),
                    InstructionValue::Primitive { value, .. } => primitive_expr(value),
                    InstructionValue::PropertyLoad { object, property, .. } => {
                        let obj = self.expr(object);
                        format!("{obj}.{property}")
                    }
                    _ => String::new(),
                }
            }
            ReactiveValue::Logical(logical) => {
                let left = self.reactive_value_expr(&logical.left);
                let right = self.reactive_value_expr(&logical.right);
                let op = match logical.operator {
                    crate::hir::hir::LogicalOperator::And => "&&",
                    crate::hir::hir::LogicalOperator::Or => "||",
                    crate::hir::hir::LogicalOperator::NullishCoalescing => "??",
                };
                format!("{left} {op} {right}")
            }
            ReactiveValue::Ternary(ternary) => {
                let test = self.reactive_value_expr(&ternary.test);
                let consequent = self.reactive_value_expr(&ternary.consequent);
                let alternate = self.reactive_value_expr(&ternary.alternate);
                format!("{test} ? {consequent} : {alternate}")
            }
            // Sequence: all intermediate instructions are transparent temps; yield the final value.
            ReactiveValue::Sequence(seq) => self.reactive_value_expr(&seq.value),
            ReactiveValue::OptionalCall(_) => String::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Free helpers
// ---------------------------------------------------------------------------

/// Strip a trailing natural `continue;` from a loop body string.
/// The last statement in a loop body is often `continue;` (the natural loop-back
/// edge). This is semantically redundant and should not be emitted.
fn strip_trailing_continue(body: &str, indent: usize) -> String {
    let pad = "  ".repeat(indent);
    let continue_line = format!("{pad}continue;\n");
    // Find the last non-empty line and check if it's `continue;`.
    if body.ends_with(&continue_line) {
        // Strip the trailing continue line.
        let trimmed = &body[..body.len() - continue_line.len()];
        trimmed.to_string()
    } else {
        body.to_string()
    }
}

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
/// Returns true if `name` is a React hook name.
/// React hooks: names starting with "use" followed by an uppercase letter (e.g. "useState"),
/// OR exactly "use" (the React 19 `use()` operator).
fn is_react_hook_name(name: &str) -> bool {
    if name == "use" {
        return true;
    }
    name.starts_with("use")
        && name.len() > 3
        && name[3..].starts_with(|c: char| c.is_uppercase())
}

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
/// Normalize arrow expression bodies: `=> {\n  return EXPR;\n}` → `=> EXPR`.
/// Applies when the arrow body has a single return statement.
fn normalize_arrow_expr_body(src: &str) -> String {
    // Find `=> {` pattern (possibly with whitespace)
    if let Some(arrow_pos) = src.find("=>") {
        let after_arrow = &src[arrow_pos + 2..].trim_start();
        if after_arrow.starts_with('{') {
            let body = &after_arrow[1..]; // skip `{`
            let body = body.trim();
            // Check if body is exactly `return EXPR;` followed by `}`
            if body.starts_with("return ") {
                let rest = &body[7..]; // after "return "
                // Find the closing `}` by counting braces
                let mut depth = 1i32;
                let chars: Vec<char> = rest.chars().collect();
                let mut semi_pos = None;
                let mut close_pos = None;
                for (i, &ch) in chars.iter().enumerate() {
                    match ch {
                        '{' => depth += 1,
                        '}' => {
                            depth -= 1;
                            if depth == 0 {
                                close_pos = Some(i);
                                break;
                            }
                        }
                        ';' if depth == 1 => {
                            if semi_pos.is_none() {
                                semi_pos = Some(i);
                            }
                        }
                        _ => {}
                    }
                }
                if let (Some(semi), Some(close)) = (semi_pos, close_pos) {
                    // Check that between semi and close is only whitespace
                    let between: String = chars[semi + 1..close].iter().collect();
                    if between.trim().is_empty() {
                        // Single return statement: convert to expression body
                        let expr: String = chars[..semi].iter().collect();
                        let prefix = &src[..arrow_pos + 2]; // everything up to and including `=>`
                        return format!("{} {}", prefix, expr.trim());
                    }
                }
            }
        }
    }
    src.to_string()
}

/// Collapse whitespace-only JSX children to self-closing: `> </Tag>` → ` />`.
fn normalize_jsx_self_closing(src: &str) -> String {
    let bytes = src.as_bytes();
    let mut result = String::with_capacity(src.len());
    let mut i = 0;
    while i < bytes.len() {
        // Look for `> </Tag>` or `></Tag>` pattern (empty children → self-closing)
        if bytes[i] == b'>' {
            // Check for optional space then `</`
            let after = i + 1;
            let (skip_space, close_start) = if after < bytes.len() && bytes[after] == b' '
                && after + 1 < bytes.len() && bytes[after + 1] == b'<'
                && after + 2 < bytes.len() && bytes[after + 2] == b'/'
            {
                (true, after + 3)
            } else if after < bytes.len() && bytes[after] == b'<'
                && after + 1 < bytes.len() && bytes[after + 1] == b'/'
            {
                (false, after + 2)
            } else {
                (false, 0)
            };
            if close_start > 0 {
                if let Some(end_off) = src[close_start..].find('>') {
                    let tag = &src[close_start..close_start + end_off];
                    if !tag.is_empty() && tag.as_bytes()[0].is_ascii_alphabetic()
                        && tag.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.')
                    {
                        result.push_str(" />");
                        i = close_start + end_off + 1;
                        continue;
                    }
                }
            }
        }
        result.push(bytes[i] as char);
        i += 1;
    }
    result
}

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
    // Strip bare blocks: `{ stmt; }` → `stmt;` (TS compiler normalizes these away)
    result = strip_bare_blocks(&result);
    // Promote `let` → `const` for variables that are never reassigned in the body.
    // The TS compiler's rename_variables pass does this for inner function declarations.
    result = promote_let_to_const(&result);
    result
}

/// Remove bare block statements: `{ ... }` not preceded by control flow keywords.
/// E.g. `function () { { console.log(z); } }` → `function () { console.log(z); }`
fn strip_bare_blocks(src: &str) -> String {
    // We do a multi-pass approach: find bare `{` not preceded by control-flow keywords,
    // find the matching `}`, and remove both, repeating until stable.
    let control_keywords: &[&str] = &[
        "if", "else", "for", "while", "do", "try", "catch", "finally", "switch", "=>",
    ];
    let mut result = src.to_string();
    loop {
        let new = strip_bare_blocks_once(&result, control_keywords);
        if new == result {
            break;
        }
        result = new;
    }
    result
}

fn strip_bare_blocks_once(src: &str, control_keywords: &[&str]) -> String {
    let bytes = src.as_bytes();
    let len = bytes.len();
    let mut i = 0;
    let mut result = String::with_capacity(len);

    while i < len {
        if bytes[i] == b'{' {
            // Check what precedes this `{` (skipping whitespace/newlines)
            let mut j = i;
            // Skip back over whitespace
            while j > 0 && (bytes[j - 1] == b' ' || bytes[j - 1] == b'\t' || bytes[j - 1] == b'\n' || bytes[j - 1] == b'\r') {
                j -= 1;
            }
            // Check if preceded by a control flow keyword or `)`
            let preceded_by_control = if j == 0 {
                false
            } else if bytes[j - 1] == b')' || bytes[j - 1] == b'>' {
                // ) from if/for/while condition, or => arrow
                true
            } else {
                // Check for keywords ending at j
                let mut found = false;
                for kw in control_keywords {
                    let klen = kw.len();
                    if j >= klen {
                        let slice = &bytes[j - klen..j];
                        if slice == kw.as_bytes() {
                            // Word boundary before keyword
                            let before_ok = j == klen || {
                                let b = bytes[j - klen - 1];
                                !b.is_ascii_alphanumeric() && b != b'_' && b != b'$'
                            };
                            if before_ok {
                                found = true;
                                break;
                            }
                        }
                    }
                }
                found
            };

            if !preceded_by_control {
                // Try to find the matching `}` for this bare block
                if let Some(close_pos) = find_matching_brace(bytes, i) {
                    // Check that this `{...}` is at statement level by verifying
                    // the content doesn't look like an object literal
                    // Simple heuristic: if the block only contains statements (no `:`
                    // at the top level that would indicate an object), strip it.
                    // For safety, only strip if the `{` is immediately followed by
                    // whitespace/newline (block statement) not something like `{key: val}`
                    let after_open = i + 1;
                    let content_start = after_open;
                    let is_block_stmt = after_open < len && (
                        bytes[after_open] == b'\n' || bytes[after_open] == b'\r' ||
                        bytes[after_open] == b' ' || bytes[after_open] == b'\t'
                    );
                    // Also make sure it's not an object literal: no `:` at depth 0 in content
                    if is_block_stmt {
                        let content = &bytes[content_start..close_pos];
                        if !looks_like_object_literal(content) {
                            // Strip the `{` and `}` and emit just the content
                            result.push_str(std::str::from_utf8(content).unwrap_or(""));
                            i = close_pos + 1;
                            continue;
                        }
                    }
                }
            }
        }

        // Handle strings to avoid matching braces inside strings
        if bytes[i] == b'"' || bytes[i] == b'\'' || bytes[i] == b'`' {
            let q = bytes[i];
            result.push(bytes[i] as char);
            i += 1;
            while i < len {
                if bytes[i] == b'\\' {
                    result.push(bytes[i] as char);
                    i += 1;
                    if i < len { result.push(bytes[i] as char); i += 1; }
                    continue;
                }
                result.push(bytes[i] as char);
                let c = bytes[i];
                i += 1;
                if c == q { break; }
            }
            continue;
        }

        result.push(bytes[i] as char);
        i += 1;
    }
    result
}

/// Find the matching `}` for an opening `{` at position `open` in `bytes`.
fn find_matching_brace(bytes: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0i32;
    let mut i = open;
    let len = bytes.len();
    while i < len {
        match bytes[i] {
            b'"' | b'\'' | b'`' => {
                let q = bytes[i];
                i += 1;
                while i < len {
                    if bytes[i] == b'\\' { i += 2; continue; }
                    if bytes[i] == q { i += 1; break; }
                    i += 1;
                }
            }
            b'{' => { depth += 1; i += 1; }
            b'}' => {
                depth -= 1;
                if depth == 0 { return Some(i); }
                i += 1;
            }
            _ => { i += 1; }
        }
    }
    None
}

/// Returns true if the byte slice looks like an object literal content (has `key: value`
/// pattern at depth 0, where `:` is not part of a ternary expression).
/// Used to avoid stripping `{...}` that are actually object literals.
fn looks_like_object_literal(content: &[u8]) -> bool {
    let mut depth = 0i32;
    let mut pending_question_at_depth0 = false;
    let mut i = 0;
    let len = content.len();
    while i < len {
        match content[i] {
            b'"' | b'\'' | b'`' => {
                let q = content[i];
                i += 1;
                while i < len {
                    if content[i] == b'\\' { i += 2; continue; }
                    if content[i] == q { i += 1; break; }
                    i += 1;
                }
            }
            b'{' | b'[' | b'(' => { depth += 1; i += 1; }
            b'}' | b']' | b')' => { depth -= 1; i += 1; }
            b'?' if depth == 0 => {
                // Mark that we've seen a `?` at depth 0 (ternary operator)
                pending_question_at_depth0 = true;
                i += 1;
            }
            b';' if depth == 0 => {
                // `;` at depth 0 means we're in a block with statements, not an object literal
                // Reset pending_question since we're past a statement boundary
                pending_question_at_depth0 = false;
                i += 1;
            }
            b':' if depth == 0 => {
                // Check it's not `::` (namespace)
                if i + 1 < len && content[i + 1] == b':' {
                    i += 2; // skip ::
                    continue;
                }
                // If we saw a `?` before this `:` at depth 0, it's a ternary, not object key
                if pending_question_at_depth0 {
                    pending_question_at_depth0 = false;
                    i += 1;
                    continue;
                }
                // A `:` at depth 0 without a preceding `?` indicates an object key-value pair
                return true;
            }
            _ => { i += 1; }
        }
    }
    false
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
    let leading_spaces = src.len() - src.trim_start().len();
    let indent = &src[..leading_spaces];

    // Handle parenthesized params — strip TypeScript type annotations if present.
    // e.g., `(param: number) => ...` → `(param) => ...`
    // e.g., `(a: A, b: B) => ...` → `(a, b) => ...`
    if s.starts_with('(') {
        // Find the closing `)` before `=>`
        let bytes = s.as_bytes();
        let mut depth = 0i32;
        let mut close = None;
        for (idx, &b) in bytes.iter().enumerate() {
            if b == b'(' { depth += 1; }
            else if b == b')' {
                depth -= 1;
                if depth == 0 { close = Some(idx); break; }
            }
        }
        if let Some(close_idx) = close {
            let params_inner = &s[1..close_idx];
            let after_close = s[close_idx + 1..].trim_start();
            // Check this is followed by `=>`
            if after_close.starts_with("=>") {
                // Strip TypeScript type annotations from params.
                // Only strip if there's a `:` that's not inside brackets/parens/angles.
                let stripped = strip_ts_type_annotations_from_params(params_inner);
                let rest = &s[close_idx + 1..];
                return format!("{indent}({stripped}){rest}");
            }
        }
        return src.to_string();
    }

    // Handle async arrow functions.
    if s.starts_with("async ") || s.starts_with("async(") {
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
            return format!("{indent}({param}) {rest}");
        }
    }
    src.to_string()
}

/// Strip TypeScript type annotations from a parameter list string.
/// Input: the content between `(` and `)` of arrow function params.
/// e.g., "param: number" → "param"
/// e.g., "a: A, b: B" → "a, b"
/// e.g., "a, b" → "a, b" (unchanged)
/// e.g., "a: Map<string, number>" → "a" (handles generic types)
fn strip_ts_type_annotations_from_params(params: &str) -> String {
    if !params.contains(':') {
        return params.to_string();
    }
    let mut result = String::new();
    let bytes = params.as_bytes();
    let len = bytes.len();
    let mut i = 0;

    while i < len {
        // Skip leading whitespace for each param
        while i < len && bytes[i].is_ascii_whitespace() { i += 1; }
        if i >= len { break; }

        // Read the parameter name (could be destructuring, spread, etc.)
        // For simplicity, read until `:`, `,`, or end
        let param_start = i;
        let mut depth = 0i32; // track brackets/braces/parens

        // Read until we find `:` at depth 0 (type annotation), `,` at depth 0 (next param), or end
        let mut colon_pos = None;
        let mut end_pos = len;

        let mut j = i;
        while j < len {
            match bytes[j] {
                b'(' | b'[' | b'{' | b'<' => { depth += 1; j += 1; }
                b')' | b']' | b'}' | b'>' => {
                    if depth > 0 { depth -= 1; }
                    j += 1;
                }
                b':' if depth == 0 => {
                    colon_pos = Some(j);
                    j += 1;
                    break;
                }
                b',' if depth == 0 => {
                    end_pos = j;
                    break;
                }
                b'=' if depth == 0 => {
                    // Default parameter `param = default` — include as-is until `,`
                    // Skip to comma
                    while j < len && !(bytes[j] == b',' && depth == 0) {
                        match bytes[j] {
                            b'(' | b'[' | b'{' => { depth += 1; j += 1; }
                            b')' | b']' | b'}' => { if depth > 0 { depth -= 1; } j += 1; }
                            _ => { j += 1; }
                        }
                    }
                    end_pos = j;
                    break;
                }
                _ => { j += 1; }
            }
        }
        if j >= len && colon_pos.is_none() {
            end_pos = len;
        }

        let param_name = if let Some(cp) = colon_pos {
            // Has type annotation — skip from `:` to `,` or end
            let name = params[param_start..cp].trim_end();
            // Skip the type annotation
            let mut k = cp + 1;
            let mut d = 0i32;
            while k < len {
                match bytes[k] {
                    b'(' | b'[' | b'{' | b'<' => { d += 1; k += 1; }
                    b')' | b']' | b'}' | b'>' => {
                        if d > 0 { d -= 1; }
                        k += 1;
                    }
                    b',' if d == 0 => { end_pos = k; break; }
                    _ => { k += 1; }
                }
            }
            if k >= len { end_pos = len; }
            name.to_string()
        } else {
            params[param_start..end_pos].trim_end().to_string()
        };

        if !result.is_empty() {
            result.push_str(", ");
        }
        result.push_str(&param_name);

        i = end_pos;
        if i < len && bytes[i] == b',' { i += 1; } // skip comma
    }

    result
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
