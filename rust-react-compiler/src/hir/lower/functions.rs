#![allow(dead_code, unused_variables, unused_imports)]

use oxc_ast::ast::*;
use oxc_index::Idx;
use oxc_semantic::Semantic;

use crate::hir::hir::*;
use crate::error::Result;
use super::LoweringContext;

// ---------------------------------------------------------------------------
// Internal helper: build a minimal stub HIRFunction body with a single block
// that returns `undefined`.
// ---------------------------------------------------------------------------

fn make_stub_hir_body(
    ctx: &mut LoweringContext,
    loc: &SourceLocation,
    is_async: bool,
    is_generator: bool,
    id: Option<String>,
) -> HIRFunction {
    // Allocate IDs up-front via the shared environment so all IDs stay globally
    // unique across the parent and child.
    let entry_id = ctx.env.new_block_id();
    let undef_id  = ctx.env.new_temporary(SourceLocation::Generated);
    let ret_id    = ctx.env.new_temporary(SourceLocation::Generated);
    let instr_id  = ctx.env.new_instruction_id();
    let term_id   = ctx.env.new_instruction_id();

    let undef_place = Place::new(undef_id, SourceLocation::Generated);
    let ret_place   = Place::new(ret_id,   SourceLocation::Generated);

    // Single entry block: `$undef = undefined; return $undef`
    let entry_block = BasicBlock {
        kind: BlockKind::Block,
        id: entry_id,
        instructions: vec![Instruction {
            id: instr_id,
            lvalue: undef_place.clone(),
            value: InstructionValue::Primitive {
                value: PrimitiveValue::Undefined,
                loc: SourceLocation::Generated,
            },
            loc: SourceLocation::Generated,
            effects: None,
        }],
        terminal: Terminal::Return {
            value: undef_place,
            return_variant: ReturnVariant::Void,
            id: term_id,
            loc: SourceLocation::Generated,
            effects: None,
        },
        preds: std::collections::HashSet::new(),
        phis: vec![],
    };

    let mut hir_body = HIR::new(entry_id);
    hir_body.blocks.insert(entry_id, entry_block);

    HIRFunction {
        loc: loc.clone(),
        id,
        name_hint: None,
        fn_type: ReactFunctionType::Other,
        params: vec![],
        return_type_annotation: None,
        returns: ret_place,
        context: vec![],
        body: hir_body,
        generator: is_generator,
        async_: is_async,
        directives: vec![],
        aliasing_effects: None,
        original_source: String::new(),
        is_arrow: false,
        is_named_export: false,
        is_default_export: false,
        outer_name: None,
            reactive_block: None,
    }
}

// ---------------------------------------------------------------------------
// Helper: detect outer-scope variable captures via source text matching.
// ---------------------------------------------------------------------------

/// Returns true if `name` appears as a standalone identifier (word-boundary) in `source`.
fn source_contains_identifier(source: &str, name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let is_id_char = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b == b'$';
    let name_bytes = name.as_bytes();
    let n = name.len();
    let source_bytes = source.as_bytes();
    let slen = source_bytes.len();
    let mut i = 0;
    while i + n <= slen {
        if &source_bytes[i..i + n] == name_bytes {
            let before_ok = i == 0 || !is_id_char(source_bytes[i - 1]);
            let after_ok = i + n == slen || !is_id_char(source_bytes[i + n]);
            // Also check that the identifier is not a property access (preceded by '.').
            // e.g. in `z.a`, the `a` is a property name, not a variable reference.
            let not_property = i == 0 || source_bytes[i - 1] != b'.';
            if before_ok && after_ok && not_property {
                // Check if this identifier is a property key or labeled statement:
                // `{name:` or `name:` at the start of a line. If the character immediately
                // after the name (skipping whitespace) is `:` and NOT `::`, this is NOT a
                // variable reference — it's an object property key or labeled statement.
                let mut j = i + n;
                while j < slen && (source_bytes[j] == b' ' || source_bytes[j] == b'\t') { j += 1; }
                let followed_by_colon = j < slen && source_bytes[j] == b':'
                    && (j + 1 >= slen || source_bytes[j + 1] != b':'); // not ::
                if followed_by_colon {
                    i += 1;
                    continue; // skip — property key or label
                }
                return true;
            }
        }
        i += 1;
    }
    false
}

/// Collect parameter names of inner arrow functions and function expressions within `source`.
/// These names shadow outer bindings and must NOT be treated as captures.
/// Handles: `name =>`, `(name, ...) =>`, `function(name) {...}`, `function name(p) {...}`.
fn collect_inner_params(source: &str) -> std::collections::HashSet<String> {
    let mut params = std::collections::HashSet::new();
    let is_id_char = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b == b'$';
    let is_id_start = |b: u8| b.is_ascii_alphabetic() || b == b'_' || b == b'$';
    let bytes = source.as_bytes();
    let len = bytes.len();
    let mut i = 0;

    while i < len {
        // Pattern 1: `IDENT =>` — single-param arrow without parens.
        // Find an identifier followed by optional whitespace and `=>`.
        if is_id_start(bytes[i]) {
            // Check word boundary before
            let at_boundary = i == 0 || !is_id_char(bytes[i - 1]);
            if at_boundary {
                let id_start = i;
                while i < len && is_id_char(bytes[i]) { i += 1; }
                let id = &source[id_start..i];
                // Skip whitespace
                let mut j = i;
                while j < len && (bytes[j] == b' ' || bytes[j] == b'\t' || bytes[j] == b'\n') { j += 1; }
                if j + 1 < len && bytes[j] == b'=' && bytes[j + 1] == b'>' {
                    // Only if not `!=` or `>=`
                    let before_eq = if j > 0 { bytes[j - 1] } else { b' ' };
                    if before_eq != b'!' && before_eq != b'>' && before_eq != b'<' {
                        params.insert(id.to_string());
                    }
                }
                continue;
            }
        }
        i += 1;
    }
    params
}

/// Collect variable names declared with `let`, `const`, or `var` inside the function body.
/// These shadow outer bindings and must NOT be treated as captures.
/// Only does simple token-level scanning (skips string literals and comments).
fn collect_local_declarations(source: &str) -> std::collections::HashSet<String> {
    let mut locals = std::collections::HashSet::new();
    let is_id_char = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b == b'$';
    let is_id_start = |b: u8| b.is_ascii_alphabetic() || b == b'_' || b == b'$';
    let bytes = source.as_bytes();
    let len = bytes.len();
    let mut i = 0;

    while i < len {
        // Skip string literals.
        if bytes[i] == b'\'' || bytes[i] == b'"' || bytes[i] == b'`' {
            let q = bytes[i];
            i += 1;
            while i < len {
                if bytes[i] == b'\\' { i += 2; continue; }
                if bytes[i] == q { i += 1; break; }
                i += 1;
            }
            continue;
        }
        // Skip line comments.
        if i + 1 < len && bytes[i] == b'/' && bytes[i + 1] == b'/' {
            while i < len && bytes[i] != b'\n' { i += 1; }
            continue;
        }
        // Skip block comments.
        if i + 1 < len && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < len && !(bytes[i] == b'*' && bytes[i + 1] == b'/') { i += 1; }
            if i + 1 < len { i += 2; }
            continue;
        }
        // Check for `let`, `const`, `var` keyword at word boundary.
        if is_id_start(bytes[i]) {
            let at_boundary = i == 0 || !is_id_char(bytes[i - 1]);
            if at_boundary {
                let id_start = i;
                while i < len && is_id_char(bytes[i]) { i += 1; }
                let kw = &source[id_start..i];
                if matches!(kw, "let" | "const" | "var") {
                    // Skip whitespace
                    while i < len && (bytes[i] == b' ' || bytes[i] == b'\t') { i += 1; }
                    // Collect all declared names (handle destructuring like `let {a, b}` or `let [a, b]`
                    // by just looking for identifier tokens at word boundaries until `;` or `=`).
                    // Simple approach: collect all identifier tokens on this line until `=` or `;` or `{`-context.
                    // We just collect the first simple identifier, which covers the most common case.
                    // For array/object destructuring, we collect all identifiers before `=`.
                    let decl_start = i;
                    // Scan for identifiers in the pattern (before `=` or `;` or `\n` with no init)
                    let mut j = i;
                    let mut depth = 0i32;
                    while j < len {
                        match bytes[j] {
                            b'{' | b'[' | b'(' => { depth += 1; j += 1; }
                            b'}' | b']' | b')' => {
                                if depth > 0 { depth -= 1; j += 1; }
                                else { break; }
                            }
                            b'=' if j + 1 < len && bytes[j + 1] != b'>' => { break; }
                            b';' | b'\n' if depth == 0 => { break; }
                            _ => { j += 1; }
                        }
                    }
                    // Extract all identifiers from `decl_start..j`
                    let pattern_src = &source[decl_start..j];
                    let pbytes = pattern_src.as_bytes();
                    let plen = pbytes.len();
                    let mut k = 0;
                    while k < plen {
                        if is_id_start(pbytes[k]) && (k == 0 || !is_id_char(pbytes[k - 1])) {
                            let istart = k;
                            while k < plen && is_id_char(pbytes[k]) { k += 1; }
                            let ident = &pattern_src[istart..k];
                            // Exclude keywords and special values
                            if !matches!(ident, "undefined" | "null" | "true" | "false" | "this" | "new" | "return") {
                                locals.insert(ident.to_string());
                            }
                        } else {
                            k += 1;
                        }
                    }
                    i = j;
                    continue;
                }
                continue;
            }
        }
        i += 1;
    }
    locals
}

/// Collect all outer-scope variables that appear (by name) in `fn_source`.
/// `excluded_params` — names of the function's own parameters (they shadow
/// outer bindings and must NOT be treated as captures).
/// Returns Places referencing the outer HIR identifiers.
fn collect_captures(ctx: &LoweringContext, fn_source: &str, excluded_params: &std::collections::HashSet<String>) -> Vec<Place> {
    // Also exclude params of inner arrows within the source (they shadow outer bindings).
    let inner_params = collect_inner_params(fn_source);
    // Exclude locally declared variables (let/const/var) within the function body.
    let local_decls = collect_local_declarations(fn_source);

    let mut seen = std::collections::HashSet::new();
    let mut result = Vec::new();
    for (&_sym_id, &ident_id) in &ctx.symbol_map {
        if seen.contains(&ident_id.0) {
            continue;
        }
        if let Some(ident) = ctx.env.get_identifier(ident_id) {
            let name = match &ident.name {
                Some(n) => n.value().to_string(),
                None => continue,
            };
            // Skip names that are parameters of this function or inner functions,
            // or locally declared variables (let/const/var) that shadow outer bindings.
            if excluded_params.contains(&name) || inner_params.contains(&name) || local_decls.contains(&name) {
                continue;
            }
            if source_contains_identifier(fn_source, &name) {
                seen.insert(ident_id.0);
                let loc = ident.loc.clone();
                if std::env::var("RC_DEBUG").is_ok() {
                    eprintln!("[capture] found capture: '{}' (id={})", name, ident_id.0);
                }
                result.push(Place::new(ident_id, loc));
            }
        }
    }
    result
}

// ---------------------------------------------------------------------------
// lower_function_expr
//
// Lowers a named or anonymous `function` expression to an
// `InstructionValue::FunctionExpression` with `fn_type: Expression`.
//
// A full recursive lowering of the body is deferred; for now a stub HIR body
// is emitted.  core.rs will wire up real body lowering once all agent modules
// are merged and circular-dependency concerns are resolved.
// ---------------------------------------------------------------------------

pub fn lower_function_expr<'a>(
    ctx: &mut LoweringContext,
    semantic: &Semantic<'a>,
    func: &Function<'a>,
    lower_expr: &mut dyn FnMut(&Expression<'a>, &mut LoweringContext) -> Result<Place>,
) -> Result<Place> {
    let loc = SourceLocation::source(func.span.start, func.span.end);
    let name = func.id.as_ref().map(|id| id.name.to_string());

    if func.generator {
        return Err(crate::error::CompilerError::todo(
            "(BuildHIR::lowerExpression) Handle YieldExpression expressions",
        ));
    }

    // Capture the original source text of the function expression.
    let original_src = semantic.source_text()
        .get(func.span.start as usize..func.span.end as usize)
        .unwrap_or("")
        .to_string();

    // Collect this function's own parameter names so they're excluded from capture detection.
    let param_names: std::collections::HashSet<String> = func.params.items.iter()
        .filter_map(|p| match &p.pattern.kind {
            BindingPatternKind::BindingIdentifier(bi) => Some(bi.name.to_string()),
            _ => None,
        })
        .collect();

    let mut lowered_fn = make_stub_hir_body(
        ctx,
        &loc,
        func.r#async,
        func.generator,
        name.clone(),
    );
    lowered_fn.original_source = original_src.clone();
    lowered_fn.context = collect_captures(ctx, &original_src, &param_names);

    let result = ctx.push(
        InstructionValue::FunctionExpression {
            name,
            name_hint: None,
            lowered_func: LoweredFunction { func: Box::new(lowered_fn) },
            fn_type: FunctionExpressionType::Expression,
            loc: loc.clone(),
        },
        loc,
    );

    Ok(result)
}

// ---------------------------------------------------------------------------
// lower_arrow
//
// Lowers an arrow function expression to an
// `InstructionValue::FunctionExpression` with `fn_type: Arrow`.
//
// Arrow functions are always anonymous and never generators.
// ---------------------------------------------------------------------------

pub fn lower_arrow<'a>(
    ctx: &mut LoweringContext,
    semantic: &Semantic<'a>,
    expr: &ArrowFunctionExpression<'a>,
    lower_expr: &mut dyn FnMut(&Expression<'a>, &mut LoweringContext) -> Result<Place>,
) -> Result<Place> {
    let loc = SourceLocation::source(expr.span.start, expr.span.end);

    // Capture the original source text of the arrow function.
    let original_src = semantic.source_text()
        .get(expr.span.start as usize..expr.span.end as usize)
        .unwrap_or("")
        .to_string();

    // Collect this arrow's own parameter names so they're excluded from capture detection.
    let param_names: std::collections::HashSet<String> = expr.params.items.iter()
        .filter_map(|p| match &p.pattern.kind {
            BindingPatternKind::BindingIdentifier(bi) => Some(bi.name.to_string()),
            _ => None,
        })
        .collect();

    let mut lowered_fn = make_stub_hir_body(
        ctx,
        &loc,
        expr.r#async,
        false, // arrows are never generators
        None,  // arrows are always anonymous
    );
    lowered_fn.original_source = original_src.clone();
    lowered_fn.context = collect_captures(ctx, &original_src, &param_names);

    let result = ctx.push(
        InstructionValue::FunctionExpression {
            name: None,
            name_hint: None,
            lowered_func: LoweredFunction { func: Box::new(lowered_fn) },
            fn_type: FunctionExpressionType::Arrow,
            loc: loc.clone(),
        },
        loc,
    );

    Ok(result)
}

// ---------------------------------------------------------------------------
// lower_function_declaration
//
// Lowers a function declaration statement.  Unlike an expression, a
// declaration binds the function value to the declared name in the current
// scope via a `StoreLocal` instruction with `InstructionKind::Function`.
//
// Steps:
//   1. Lower the function itself (reusing lower_function_expr).
//   2. Resolve the binding identifier via oxc's semantic symbol table.
//   3. Emit StoreLocal { lvalue: LValue { place, kind: Function }, value }.
// ---------------------------------------------------------------------------

pub fn lower_function_declaration<'a>(
    ctx: &mut LoweringContext,
    semantic: &Semantic<'a>,
    func: &Function<'a>,
    lower_expr: &mut dyn FnMut(&Expression<'a>, &mut LoweringContext) -> Result<Place>,
) -> Result<()> {
    let loc = SourceLocation::source(func.span.start, func.span.end);

    // 1. Build the FunctionExpression instruction (stub body).
    let func_place = lower_function_expr(ctx, semantic, func, lower_expr)?;

    // 2. Resolve or create the HIR identifier for the declaration name.
    //    Function declarations always have an `id`; if somehow missing we
    //    just skip the StoreLocal and return the value unreferenced.
    let Some(func_id) = func.id.as_ref() else {
        return Ok(());
    };

    // Look up the oxc SymbolId for this binding.  If the identifier has no
    // symbol (possible in pathological/error-recovery parses) we skip.
    let Some(symbol_id) = func_id.symbol_id.get() else {
        return Ok(());
    };

    let ident_id = ctx.get_or_create_symbol(
        symbol_id.index() as u32,
        Some(func_id.name.as_str()),
        loc.clone(),
    );
    let lvalue_place = Place::new(ident_id, loc.clone());

    // 3. Emit StoreLocal binding the function value to the declared name.
    ctx.push_with_lvalue(
        lvalue_place.clone(),
        InstructionValue::StoreLocal {
            lvalue: LValue {
                place: lvalue_place,
                kind: InstructionKind::Function,
            },
            value: func_place,
            type_annotation: None,
            loc: loc.clone(),
        },
        loc,
    );

    Ok(())
}
