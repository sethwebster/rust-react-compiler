/// Outline pure (no-capture) arrow functions as top-level
/// `function _temp(...)` declarations.
///
/// A function can be outlined only if ALL its free variables are module-level
/// globals/imports (accessed via LoadGlobal). Any component-local capture
/// (even a stable one like `ref = useRef(null)`) prevents outlining.
///
/// This mirrors OutlineFunctions.ts in the TypeScript React compiler.
use std::collections::HashSet;

use crate::hir::environment::Environment;
use crate::hir::hir::{FunctionExpressionType, HIRFunction, InstructionKind, InstructionValue, NonLocalBinding, PrimitiveValue};

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

pub fn outline_functions(hir: &mut HIRFunction, env: &mut Environment) {
    // Build the set of names that are module-level (LoadGlobal of any variant,
    // plus module-level variable declarations collected during lowering).
    // Free variables that appear in this set are safe to reference from an
    // outlined (hoisted) function.
    let mut module_names: HashSet<String> = HashSet::new();
    for (_, block) in &hir.body.blocks {
        for instr in &block.instructions {
            if let InstructionValue::LoadGlobal { binding, .. } = &instr.value {
                let name = match binding {
                    NonLocalBinding::Global { name } => name.clone(),
                    NonLocalBinding::ImportDefault { name, .. } => name.clone(),
                    NonLocalBinding::ImportNamespace { name, .. } => name.clone(),
                    NonLocalBinding::ImportSpecifier { name, .. } => name.clone(),
                    NonLocalBinding::ModuleLocal { name } => name.clone(),
                };
                module_names.insert(name);
            }
        }
    }
    // Also include module-level variable declarations (let/const/var at module scope).
    // These are stable from the component's perspective — they don't change per render.
    module_names.extend(env.module_level_names.iter().cloned());

    // Collect all named local variables in the outer (component) scope.
    // Used to detect when an outlined function's param shadows an outer variable.
    let mut outer_local_names: HashSet<String> = HashSet::new();
    for (id, ident) in &env.identifiers {
        if let Some(name) = &ident.name {
            let n = name.value().to_string();
            outer_local_names.insert(n);
        }
        let _ = id;
    }

    // Collect FunctionExpression lvalue IDs that are immediately called (IIFEs).
    // These must NOT be outlined — they should be inlined or left as-is.
    let mut iife_fn_ids: std::collections::HashSet<u32> = std::collections::HashSet::new();
    for (_, block) in &hir.body.blocks {
        for instr in &block.instructions {
            if let InstructionValue::CallExpression { callee, args, .. } = &instr.value {
                if args.is_empty() {
                    iife_fn_ids.insert(callee.identifier.0);
                }
            }
        }
    }

    // Build a map of local name → global name for const locals assigned from globals.
    // This allows outlining functions that capture such locals by replacing them
    // with the global name in the outlined body.
    let mut const_local_to_global: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    {
        // Build ident_id → global name from LoadGlobal instructions.
        let mut id_to_global: std::collections::HashMap<u32, String> = std::collections::HashMap::new();
        for (_, block) in &hir.body.blocks {
            for instr in &block.instructions {
                if let InstructionValue::LoadGlobal { binding, .. } = &instr.value {
                    let name = match binding {
                        NonLocalBinding::Global { name } => name.clone(),
                        NonLocalBinding::ImportDefault { name, .. } => name.clone(),
                        NonLocalBinding::ImportNamespace { name, .. } => name.clone(),
                        NonLocalBinding::ImportSpecifier { name, .. } => name.clone(),
                        NonLocalBinding::ModuleLocal { name } => name.clone(),
                    };
                    id_to_global.insert(instr.lvalue.identifier.0, name);
                }
            }
        }
        // Find StoreLocal(const, local_name, value) where value → global.
        for (_, block) in &hir.body.blocks {
            for instr in &block.instructions {
                if let InstructionValue::StoreLocal { lvalue, value, .. } = &instr.value {
                    if matches!(lvalue.kind, crate::hir::hir::InstructionKind::Const) {
                        if let Some(global_name) = id_to_global.get(&value.identifier.0) {
                            if let Some(local_name) = env.get_identifier(lvalue.place.identifier)
                                .and_then(|i| i.name.as_ref())
                                .map(|n| n.value().to_string())
                            {
                                const_local_to_global.insert(local_name, global_name.clone());
                            }
                        }
                    }
                }
            }
        }
    }

    // Build a map of local name → primitive value for const locals assigned from primitives.
    // This allows outlining functions that capture `const x = 42;` by inlining the literal.
    // Also track flat instruction indices to detect hoisting (const defined after function).
    let mut const_name_to_primitive: std::collections::HashMap<String, PrimitiveValue> = std::collections::HashMap::new();
    // Maps local const name → flat instruction index of its StoreLocal.
    let mut const_name_to_instr_idx: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    // Maps FunctionExpression lvalue identifier.0 → flat instruction index.
    let mut fn_expr_to_instr_idx: std::collections::HashMap<u32, usize> = std::collections::HashMap::new();
    {
        // Build ident_id → (primitive value, flat index) from Primitive instructions.
        let mut id_to_prim: std::collections::HashMap<u32, PrimitiveValue> = std::collections::HashMap::new();
        let mut flat_idx: usize = 0;
        for (_, block) in &hir.body.blocks {
            for instr in &block.instructions {
                if let InstructionValue::Primitive { value, .. } = &instr.value {
                    id_to_prim.insert(instr.lvalue.identifier.0, value.clone());
                }
                if matches!(instr.value, InstructionValue::FunctionExpression { .. }) {
                    fn_expr_to_instr_idx.insert(instr.lvalue.identifier.0, flat_idx);
                }
                flat_idx += 1;
            }
        }
        // Find StoreLocal(const, local_name, value) where value → primitive.
        flat_idx = 0;
        for (_, block) in &hir.body.blocks {
            for instr in &block.instructions {
                if let InstructionValue::StoreLocal { lvalue, value, .. } = &instr.value {
                    if matches!(lvalue.kind, InstructionKind::Const | InstructionKind::HoistedConst) {
                        if let Some(prim) = id_to_prim.get(&value.identifier.0) {
                            if let Some(local_name) = env.get_identifier(lvalue.place.identifier)
                                .and_then(|i| i.name.as_ref())
                                .map(|n| n.value().to_string())
                            {
                                const_name_to_primitive.insert(local_name.clone(), prim.clone());
                                const_name_to_instr_idx.insert(local_name, flat_idx);
                            }
                        }
                    }
                }
                flat_idx += 1;
            }
        }
    }

    let mut temp_counter = 0usize;

    // Deduplication map: canonical signature → assigned temp name.
    // If two outlined functions have identical params + body, they share a name.
    let mut outlined_dedup: std::collections::HashMap<String, String> = std::collections::HashMap::new();

    // If @enableNameAnonymousFunctions is set, build a map from FunctionExpression
    // lvalue identifier id → the named variable it flows into (via StoreLocal).
    // This lets us generate meaningful names like `_ComponentOnClick` instead of `_temp`.
    let enable_named = env.config.enable_name_anonymous_functions;
    let component_name = hir.id.as_deref().unwrap_or("").to_string();
    let mut fn_id_to_var_name: std::collections::HashMap<u32, String> = std::collections::HashMap::new();
    if enable_named && !component_name.is_empty() {
        for (_, block) in &hir.body.blocks {
            for instr in &block.instructions {
                if let InstructionValue::StoreLocal { lvalue, value, .. } = &instr.value {
                    if let Some(lv_name) = env.get_identifier(lvalue.place.identifier)
                        .and_then(|id| id.name.as_ref())
                        .map(|n| n.value().to_string())
                    {
                        fn_id_to_var_name.insert(value.identifier.0, lv_name);
                    }
                }
            }
        }
    }

    for (_, block) in &mut hir.body.blocks {
        for instr in &mut block.instructions {
            if let InstructionValue::FunctionExpression {
                name_hint,
                lowered_func,
                fn_type,
                ..
            } = &mut instr.value
            {
                let src = lowered_func.func.original_source.clone();
                if src.is_empty() {
                    continue;
                }

                // Parse the function to get param names and body text.
                let info = if *fn_type == FunctionExpressionType::Arrow {
                    match parse_arrow_info(&src) {
                        Some(i) => i,
                        None => continue,
                    }
                } else {
                    match parse_function_expr_info(&src) {
                        Some(i) => i,
                        None => continue,
                    }
                };

                // Skip IIFEs — they are immediately called and should not be outlined themselves.
                // But still try to outline any pure inner arrow functions found in the IIFE's source text.
                if iife_fn_ids.contains(&instr.lvalue.identifier.0) {
                    let outer_caps: HashSet<String> = lowered_func.func.context.iter()
                        .filter_map(|p| env.get_identifier(p.identifier)
                            .and_then(|id| id.name.as_ref())
                            .map(|n| n.value().to_string()))
                        .collect();
                    let mut forbidden: HashSet<String> = outer_caps;
                    forbidden.extend(info.param_names.iter().cloned());
                    forbidden.extend(outer_local_names.iter().cloned());
                    forbidden.retain(|n| !module_names.contains(n)
                        && !const_local_to_global.contains_key(n)
                        && !const_name_to_primitive.contains_key(n));
                    let patched = outline_inner_arrows_in_source(
                        &src, &info.param_names, &forbidden,
                        &outer_local_names, &module_names,
                        &const_local_to_global, &const_name_to_primitive,
                        env, &mut temp_counter,
                    );
                    if patched != src {
                        lowered_func.func.original_source = patched;
                    }
                    continue;
                }

                // Use the HIR context (captured variables) to check if the
                // function captures any component-local variables. The context
                // is authoritative — it correctly handles nested functions
                // whose parameters shadow outer names.
                // Match TS OutlineFunctions.ts behavior: only outline when context is empty.
                // The TS compiler checks `context.length === 0` — any captured variable
                // (including const-from-primitive locals) prevents outlining.
                // Const-from-global and module-level captures are allowed since those
                // are stable references not tied to the component instance.
                // Flat instruction index of this FunctionExpression (for hoisting detection).
                let fn_instr_idx = fn_expr_to_instr_idx.get(&instr.lvalue.identifier.0).copied().unwrap_or(usize::MAX);
                let has_local_capture = lowered_func.func.context.iter().any(|place| {
                    if let Some(name) = env.get_identifier(place.identifier)
                        .and_then(|id| id.name.as_ref())
                        .map(|n| n.value().to_string())
                    {
                        if !outer_local_names.contains(name.as_str())
                            || module_names.contains(name.as_str())
                            || const_local_to_global.contains_key(name.as_str())
                        {
                            return false;
                        }
                        // A const-from-primitive is safe to inline only if defined
                        // BEFORE the function expression (no temporal dead zone).
                        if let Some(&prim_idx) = const_name_to_instr_idx.get(name.as_str()) {
                            return prim_idx >= fn_instr_idx;
                        }
                        true
                    } else {
                        false
                    }
                });
                if has_local_capture {
                    // Cannot outline the outer function, but try to outline any
                    // inner pure arrow functions found in its source text.
                    let outer_caps: HashSet<String> = lowered_func.func.context.iter()
                        .filter_map(|p| env.get_identifier(p.identifier)
                            .and_then(|id| id.name.as_ref())
                            .map(|n| n.value().to_string()))
                        .collect();
                    let mut forbidden: HashSet<String> = outer_caps;
                    forbidden.extend(info.param_names.iter().cloned());
                    forbidden.extend(outer_local_names.iter().cloned());
                    forbidden.retain(|n| !module_names.contains(n)
                        && !const_local_to_global.contains_key(n)
                        && !const_name_to_primitive.contains_key(n));
                    let patched = outline_inner_arrows_in_source(
                        &src, &info.param_names, &forbidden,
                        &outer_local_names, &module_names,
                        &const_local_to_global, &const_name_to_primitive,
                        env, &mut temp_counter,
                    );
                    if patched != src {
                        lowered_func.func.original_source = patched;
                    }
                    continue;
                }
                // Source-text free-variable analysis for renaming const-from-global
                // captures in the outlined body (still needed for body rewriting).
                let free_vars = extract_free_variables(&info.body_text, &info.param_names);

                // Generate a unique name.
                // With @enableNameAnonymousFunctions, use `_ComponentOnClick` style names
                // (component name + capitalized variable name). Otherwise `_temp`, `_temp2`, ...
                // NOTE: Babel's generateUidIdentifier skips "_temp1" → sequence is _temp, _temp2, _temp3, ...
                let temp_name = if enable_named && !component_name.is_empty() {
                    if let Some(var_name) = fn_id_to_var_name.get(&instr.lvalue.identifier.0) {
                        // Capitalize first letter of var name.
                        let mut cap = var_name.clone();
                        if let Some(f) = cap.get_mut(0..1) { f.make_ascii_uppercase(); }
                        format!("_{}{}", component_name, cap)
                    } else {
                        if temp_counter == 0 { "_temp".to_string() } else { format!("_temp{}", temp_counter + 1) }
                    }
                } else if temp_counter == 0 {
                    "_temp".to_string()
                } else {
                    format!("_temp{}", temp_counter + 1)
                };
                temp_counter += 1;

                // Rename params that shadow outer local variables (add _0 suffix).
                // This matches the TS compiler's behavior of renaming shadowing params.
                let final_param_names: Vec<String> = info.param_names.iter().map(|p| {
                    if outer_local_names.contains(p.as_str()) {
                        format!("{}_0", p)
                    } else {
                        p.clone()
                    }
                }).collect();

                // Replace const-from-global captures with their global names.
                let mut renamed_body = info.body_text.clone();
                for fv in &free_vars {
                    if let Some(global_name) = const_local_to_global.get(fv.as_str()) {
                        renamed_body = rename_word(&renamed_body, fv, global_name);
                    }
                }
                // Replace const-from-primitive captures with their literal values.
                for fv in &free_vars {
                    if let Some(prim) = const_name_to_primitive.get(fv.as_str()) {
                        let literal = primitive_to_literal(prim);
                        renamed_body = rename_word(&renamed_body, fv, &literal);
                    }
                }

                // Rename uses of renamed params in the body text.
                for (orig, renamed) in info.param_names.iter().zip(final_param_names.iter()) {
                    if orig != renamed {
                        renamed_body = rename_word(&renamed_body, orig, renamed);
                    }
                }

                // Rename catch parameters to temp names (t0, t1, ...) to match
                // the reference compiler's rename_variables behavior.
                renamed_body = rename_catch_params(&renamed_body);

                // Strip TypeScript type annotations from the body text.
                // Handles simple patterns like `let g: Foo = f;` → `let g = f;`
                // and `(arg: Type)` → `(arg)`.
                renamed_body = strip_type_annotations(&renamed_body);

                // Pre-normalize inner arrow bodies: convert `=> { return EXPR; }` to `=> EXPR`
                // for all inner arrows in the body. This prevents a bug in codegen's
                // normalize_arrow_expr_body which finds the first `=>` in the outlined function
                // text (which may be an inner arrow) and incorrectly truncates the function body.
                if !info.is_expr_body {
                    renamed_body = normalize_single_return_arrows(&renamed_body);
                }

                // TS-style normalization: convert single destructured params to t0 + explicit
                // destructuring at top of body. e.g. `({id, name}) => <C />` becomes
                // `function _temp(t0) { const {id, name} = t0; return <C />; }`
                let (final_params_for_decl, destructuring_prefix) = if
                    info.total_param_count == 1
                    && info.destructured_params.len() == 1
                    && info.destructured_params[0].0 == 0
                {
                    let raw_pattern = &info.destructured_params[0].1;
                    let prefix = format!("  const {} = t0;\n", raw_pattern);
                    (vec!["t0".to_string()], prefix)
                } else {
                    (final_param_names.clone(), String::new())
                };

                // Build the function declaration.
                let params_str = final_params_for_decl.join(", ");

                // Deduplication: if an identical function was already outlined, reuse its name.
                let sig = format!("{}|{}|{}", params_str, destructuring_prefix, renamed_body);
                if let Some(existing_name) = outlined_dedup.get(&sig) {
                    *name_hint = Some(existing_name.clone());
                    lowered_func.func.context.clear();
                    continue;
                }
                // For arrow functions with expression bodies, strip outer parens from the
                // body text before wrapping in `return ...`. An arrow like `(x) => ({...x})`
                // has body text `({...x})` — the parens are syntactically required in arrow
                // position but become redundant when wrapped in a `return` statement.
                let return_body = if info.is_expr_body
                    && renamed_body.starts_with('(')
                    && renamed_body.ends_with(')')
                {
                    let inner = &renamed_body[1..renamed_body.len() - 1];
                    let balanced = inner.chars().fold(0i32, |d, c| match c {
                        '(' => d + 1,
                        ')' => d - 1,
                        _ => d,
                    }) == 0;
                    if balanced {
                        inner.to_string()
                    } else {
                        renamed_body.clone()
                    }
                } else {
                    renamed_body.clone()
                };
                let decl = if info.is_expr_body {
                    format!(
                        "function {}({}) {{\n{}  return {};\n}}",
                        temp_name, params_str, destructuring_prefix, return_body
                    )
                } else if !destructuring_prefix.is_empty() {
                    // Block body: inject destructuring after opening `{`.
                    // renamed_body starts with `{` and ends with `}`.
                    if renamed_body.starts_with('{') {
                        let inner = &renamed_body[1..renamed_body.len()-1];
                        format!("function {}({}) {{\n{}{}}}", temp_name, params_str, destructuring_prefix, inner)
                    } else {
                        format!("function {}({}) {}", temp_name, params_str, renamed_body)
                    }
                } else {
                    // Block body already includes `{ ... }`.
                    format!("function {}({}) {}", temp_name, params_str, renamed_body)
                };

                // Mark this instruction as outlined so codegen emits just the name.
                *name_hint = Some(temp_name.clone());

                // Clear the function's context so DCE knows this function no longer
                // captures any component-local variables.
                lowered_func.func.context.clear();

                outlined_dedup.insert(sig, temp_name.clone());
                env.outlined_functions.push((temp_name, decl));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Arrow function parser
// ---------------------------------------------------------------------------

struct ArrowInfo {
    param_names: Vec<String>,
    body_text: String,
    is_expr_body: bool,
    /// For outlined functions with destructured params: the raw text of the destructuring pattern
    /// (e.g. `{id, name}` or `[, value]`) and which positional param index it belongs to.
    /// TS normalizes these to `t0, t1, ...` parameters with explicit const destructuring in body.
    destructured_params: Vec<(usize, String)>, // (param_index, raw_pattern_text)
    /// Total number of formal parameters (not counting rest).
    total_param_count: usize,
}

/// Recursively collect all binding identifier names from a binding pattern.
/// Handles simple identifiers, object destructuring, array destructuring, and assignment patterns.
fn collect_binding_names(kind: &oxc_ast::ast::BindingPatternKind, names: &mut Vec<String>) {
    use oxc_ast::ast::BindingPatternKind;
    match kind {
        BindingPatternKind::BindingIdentifier(id) => {
            names.push(id.name.to_string());
        }
        BindingPatternKind::ObjectPattern(obj) => {
            for prop in &obj.properties {
                collect_binding_names(&prop.value.kind, names);
            }
            if let Some(rest) = &obj.rest {
                collect_binding_names(&rest.argument.kind, names);
            }
        }
        BindingPatternKind::ArrayPattern(arr) => {
            for elem in &arr.elements {
                if let Some(elem) = elem {
                    collect_binding_names(&elem.kind, names);
                }
            }
            if let Some(rest) = &arr.rest {
                collect_binding_names(&rest.argument.kind, names);
            }
        }
        BindingPatternKind::AssignmentPattern(assign) => {
            collect_binding_names(&assign.left.kind, names);
        }
    }
}

/// Parse an arrow function source text (e.g. `item => item` or `(a, b) => a + b`).
/// Returns `None` if parsing fails.
fn parse_arrow_info(src: &str) -> Option<ArrowInfo> {
    use oxc_allocator::Allocator;
    use oxc_ast::ast::{BindingPatternKind, Expression, Statement};
    use oxc_parser::Parser;
    use oxc_span::SourceType;

    let alloc = Allocator::default();
    // Wrap as a variable initializer so oxc treats it as an expression.
    let stmt_src = format!("let _ = {};", src);
    // Use tsx() so TypeScript type annotations (e.g. `(f: Foo) =>`) parse correctly.
    let parsed = Parser::new(&alloc, &stmt_src, SourceType::tsx()).parse();
    if !parsed.errors.is_empty() {
        return None;
    }

    let stmt = parsed.program.body.first()?;
    let arrow = match stmt {
        Statement::VariableDeclaration(vd) => {
            let decl = vd.declarations.first()?;
            let init = decl.init.as_ref()?;
            match init {
                Expression::ArrowFunctionExpression(a) => a.as_ref(),
                _ => return None,
            }
        }
        _ => return None,
    };

    // Collect parameter names, including from destructuring patterns.
    // Also detect destructured params (ObjectPattern/ArrayPattern) for TS-style normalization.
    let mut param_names = Vec::new();
    let mut destructured_params: Vec<(usize, String)> = Vec::new();
    let prefix_len: usize = "let _ = ".len(); // 8
    for (idx, param) in arrow.params.items.iter().enumerate() {
        use oxc_ast::ast::BindingPatternKind;
        match &param.pattern.kind {
            BindingPatternKind::ObjectPattern(_) | BindingPatternKind::ArrayPattern(_) => {
                // Record the raw pattern text for later normalization
                let pat_start = (param.span.start as usize).saturating_sub(prefix_len);
                let pat_end = (param.span.end as usize).saturating_sub(prefix_len);
                if let Some(raw) = src.get(pat_start..pat_end) {
                    destructured_params.push((idx, raw.to_string()));
                }
                collect_binding_names(&param.pattern.kind, &mut param_names);
            }
            _ => {
                collect_binding_names(&param.pattern.kind, &mut param_names);
            }
        }
    }
    if let Some(rest) = &arrow.params.rest {
        collect_binding_names(&rest.argument.kind, &mut param_names);
    }

    // Extract body text by adjusting the span for the `let _ = ` prefix (8 bytes).
    let body_span = arrow.body.span;
    let body_start = (body_span.start as usize).saturating_sub(prefix_len);
    let body_end = (body_span.end as usize).saturating_sub(prefix_len);
    let body_text = src.get(body_start..body_end)?.to_string();

    Some(ArrowInfo {
        total_param_count: arrow.params.items.len(),
        param_names,
        body_text,
        is_expr_body: arrow.expression,
        destructured_params,
    })
}

/// Parse a regular function expression: `function(a, b) { body }` or `function name(a) { body }`.
fn parse_function_expr_info(src: &str) -> Option<ArrowInfo> {
    use oxc_allocator::Allocator;
    use oxc_ast::ast::{BindingPatternKind, Expression, Statement};
    use oxc_parser::Parser;
    use oxc_span::SourceType;

    let alloc = Allocator::default();
    // Wrap in a variable so oxc treats it as an expression.
    let stmt_src = format!("let _ = {};", src);
    // Use tsx() so TypeScript type annotations parse correctly.
    let parsed = Parser::new(&alloc, &stmt_src, SourceType::tsx()).parse();
    if !parsed.errors.is_empty() {
        return None;
    }

    let stmt = parsed.program.body.first()?;
    let func_expr = match stmt {
        Statement::VariableDeclaration(vd) => {
            let decl = vd.declarations.first()?;
            let init = decl.init.as_ref()?;
            match init {
                Expression::FunctionExpression(f) => f.as_ref(),
                _ => return None,
            }
        }
        _ => return None,
    };

    let mut param_names = Vec::new();
    let mut destructured_params: Vec<(usize, String)> = Vec::new();
    let prefix_len: usize = "let _ = ".len();
    for (idx, param) in func_expr.params.items.iter().enumerate() {
        use oxc_ast::ast::BindingPatternKind;
        match &param.pattern.kind {
            BindingPatternKind::ObjectPattern(_) | BindingPatternKind::ArrayPattern(_) => {
                let pat_start = (param.span.start as usize).saturating_sub(prefix_len);
                let pat_end = (param.span.end as usize).saturating_sub(prefix_len);
                if let Some(raw) = src.get(pat_start..pat_end) {
                    destructured_params.push((idx, raw.to_string()));
                }
                collect_binding_names(&param.pattern.kind, &mut param_names);
            }
            _ => {
                collect_binding_names(&param.pattern.kind, &mut param_names);
            }
        }
    }
    if let Some(rest) = &func_expr.params.rest {
        collect_binding_names(&rest.argument.kind, &mut param_names);
    }

    let body = func_expr.body.as_ref()?;
    let body_start = (body.span.start as usize).saturating_sub(prefix_len);
    let body_end = (body.span.end as usize).saturating_sub(prefix_len);
    let body_text = src.get(body_start..body_end)?.to_string();

    Some(ArrowInfo {
        total_param_count: func_expr.params.items.len(),
        param_names,
        body_text,
        is_expr_body: false, // Regular functions always have block body.
        destructured_params,
    })
}

// ---------------------------------------------------------------------------
// Identifier helpers
// ---------------------------------------------------------------------------

/// Collect identifier tokens from `body` that are NOT params and NOT preceded by `.`
/// (property accesses). Excludes JS keywords, string literals, and comments.
fn extract_free_variables(body: &str, param_names: &[String]) -> HashSet<String> {
    let param_set: HashSet<&str> = param_names.iter().map(|s| s.as_str()).collect();
    let mut free_vars = HashSet::new();
    let bytes = body.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Skip line comments: // ...
        if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'/' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        // Skip block comments: /* ... */
        if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            if i + 1 < bytes.len() {
                i += 2;
            }
            continue;
        }
        // Skip string literals: '...', "...", `...`
        if bytes[i] == b'\'' || bytes[i] == b'"' || bytes[i] == b'`' {
            let quote = bytes[i];
            i += 1;
            while i < bytes.len() && bytes[i] != quote {
                if bytes[i] == b'\\' {
                    i += 1; // skip escaped char
                }
                i += 1;
            }
            if i < bytes.len() {
                i += 1; // skip closing quote
            }
            continue;
        }
        // Identifier token
        if bytes[i].is_ascii_alphabetic() || bytes[i] == b'_' || bytes[i] == b'$' {
            let start = i;
            while i < bytes.len()
                && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' || bytes[i] == b'$')
            {
                i += 1;
            }
            let tok = &body[start..i];
            // Property access: preceded by `.`
            let is_prop = start > 0 && bytes[start - 1] == b'.';
            // Object key: followed by `:` (skip whitespace) — e.g. `{x: value}`
            let mut j = i;
            while j < bytes.len() && bytes[j].is_ascii_whitespace() { j += 1; }
            let is_obj_key = j < bytes.len() && bytes[j] == b':';
            if !is_prop && !is_obj_key && !is_js_keyword(tok) && !param_set.contains(tok) {
                free_vars.insert(tok.to_string());
            }
        } else {
            i += 1;
        }
    }
    free_vars
}

/// Replace whole-word occurrences of `old` with `new` in `text`.
fn rename_word(text: &str, old: &str, new: &str) -> String {
    let mut result = String::new();
    let bytes = text.as_bytes();
    let old_bytes = old.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i..].starts_with(old_bytes) {
            let before_ok = i == 0 || !is_word_byte(bytes[i - 1]);
            let after_pos = i + old.len();
            let after_ok = after_pos >= bytes.len() || !is_word_byte(bytes[after_pos]);
            if before_ok && after_ok {
                result.push_str(new);
                i = after_pos;
                continue;
            }
        }
        // Append one character. Assumes source is mostly ASCII; multi-byte safe
        // because we only replace ASCII identifiers.
        result.push(bytes[i] as char);
        i += 1;
    }
    result
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$'
}

/// Rename catch parameters to temp names (t0, t1, ...) in source text.
/// Finds `catch (NAME)` or `catch(NAME)` patterns and renames the identifier + all uses.
fn rename_catch_params(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut result = text.to_string();
    let mut counter = 0u32;
    let catch_keyword = b"catch";
    let mut i = 0;
    let mut catches: Vec<String> = Vec::new();
    while i + 5 < bytes.len() {
        if &bytes[i..i + 5] == catch_keyword {
            // Check word boundary before "catch"
            if i > 0 && is_word_byte(bytes[i - 1]) {
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
                while j < bytes.len() && is_word_byte(bytes[j]) {
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
            result = rename_word(&result, &name, &new_name);
        }
    }
    result
}

/// Convert a PrimitiveValue to its JavaScript literal string representation.
fn primitive_to_literal(prim: &PrimitiveValue) -> String {
    match prim {
        PrimitiveValue::Number(n) => {
            if n.fract() == 0.0 && n.abs() < 1e15 {
                format!("{}", *n as i64)
            } else {
                format!("{}", n)
            }
        }
        PrimitiveValue::Boolean(b) => if *b { "true".to_string() } else { "false".to_string() },
        PrimitiveValue::String(s) => format!("\"{}\"", s),
        PrimitiveValue::Null => "null".to_string(),
        PrimitiveValue::Undefined => "undefined".to_string(),
    }
}

fn is_js_keyword(tok: &str) -> bool {
    matches!(
        tok,
        "break"
            | "case"
            | "catch"
            | "continue"
            | "debugger"
            | "default"
            | "delete"
            | "do"
            | "else"
            | "finally"
            | "for"
            | "function"
            | "if"
            | "in"
            | "instanceof"
            | "new"
            | "return"
            | "switch"
            | "this"
            | "throw"
            | "try"
            | "typeof"
            | "var"
            | "void"
            | "while"
            | "with"
            | "class"
            | "const"
            | "enum"
            | "export"
            | "extends"
            | "import"
            | "super"
            | "implements"
            | "interface"
            | "let"
            | "package"
            | "private"
            | "protected"
            | "public"
            | "static"
            | "yield"
            | "null"
            | "true"
            | "false"
            | "undefined"
            | "async"
            | "await"
            | "of"
            | "arguments"
    )
}

/// Strip TypeScript type annotations from source text by parsing with oxc (tsx mode)
/// and re-emitting with oxc_codegen (which drops type annotations).
/// If parsing fails, returns the input unchanged.
fn strip_type_annotations(src: &str) -> String {
    // Quick check: if no `:` in the source, there are likely no type annotations.
    if !src.contains(':') {
        return src.to_string();
    }

    use oxc_allocator::Allocator;
    use oxc_parser::Parser;
    use oxc_span::SourceType;
    use oxc_ast::ast::{Statement, Declaration, VariableDeclarationKind};

    let alloc = Allocator::default();
    // Parse the body as a function to get proper AST.
    let is_block = src.trim_start().starts_with('{');
    let wrapped = if is_block {
        format!("function _x() {}", src)
    } else {
        format!("function _x() {{ return {}; }}", src)
    };
    let parsed = Parser::new(&alloc, &wrapped, SourceType::tsx()).parse();
    if !parsed.errors.is_empty() {
        return src.to_string();
    }

    // Walk the AST to find variable declarations with type annotations.
    // For each one, find `: TypeName` between the binding name and `=` or `;`.
    // Build a list of (start, end) spans in the original source to remove.
    let mut removals: Vec<(usize, usize)> = Vec::new();
    let prefix_len = if is_block { "function _x() ".len() } else { "function _x() { return ".len() };

    fn collect_type_removals(stmts: &[Statement], prefix_len: usize, removals: &mut Vec<(usize, usize)>) {
        for stmt in stmts {
            match stmt {
                Statement::VariableDeclaration(vd) => {
                    for decl in &vd.declarations {
                        if let Some(ann) = &decl.id.type_annotation {
                            let start = ann.span.start as usize;
                            let end = ann.span.end as usize;
                            // The span includes `: Type`. Adjust for prefix.
                            if start >= prefix_len {
                                removals.push((start - prefix_len, end - prefix_len));
                            }
                        }
                    }
                }
                Statement::BlockStatement(block) => {
                    collect_type_removals(&block.body, prefix_len, removals);
                }
                Statement::IfStatement(ifs) => {
                    if let Statement::BlockStatement(b) = &ifs.consequent {
                        collect_type_removals(&b.body, prefix_len, removals);
                    }
                    if let Some(alt) = &ifs.alternate {
                        if let Statement::BlockStatement(b) = alt {
                            collect_type_removals(&b.body, prefix_len, removals);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    // Get the function body statements
    if let Some(stmt) = parsed.program.body.first() {
        if let Statement::FunctionDeclaration(fd) = stmt {
            if let Some(body) = &fd.body {
                collect_type_removals(&body.statements, prefix_len, &mut removals);
            }
        }
    }

    if removals.is_empty() {
        return src.to_string();
    }

    // Apply removals in reverse order to preserve indices.
    let mut result = src.to_string();
    removals.sort_by(|a, b| b.0.cmp(&a.0));
    for (start, end) in removals {
        if start < result.len() && end <= result.len() {
            result = format!("{}{}", &result[..start], &result[end..]);
        }
    }
    result
}

// ---------------------------------------------------------------------------
// Inner arrow function outliner (source-text level)
// ---------------------------------------------------------------------------

/// Walk the outer function's source text, find inner pure arrow functions
/// (those whose free variables don't include any `forbidden` names), outline
/// them, and return the patched source. If nothing changed, returns original.
#[allow(clippy::too_many_arguments)]
fn outline_inner_arrows_in_source(
    src: &str,
    outer_param_names: &[String],
    forbidden: &HashSet<String>,
    outer_local_names: &HashSet<String>,
    module_names: &HashSet<String>,
    const_local_to_global: &std::collections::HashMap<String, String>,
    const_name_to_primitive: &std::collections::HashMap<String, PrimitiveValue>,
    env: &mut crate::hir::environment::Environment,
    temp_counter: &mut usize,
) -> String {
    use oxc_allocator::Allocator;
    use oxc_ast::ast::{Expression, Statement};
    use oxc_parser::Parser;
    use oxc_span::SourceType;

    let alloc = Allocator::default();
    let stmt_src = format!("let _ = {};", src);
    let parsed = Parser::new(&alloc, &stmt_src, SourceType::tsx()).parse();
    if !parsed.errors.is_empty() {
        return src.to_string();
    }
    let prefix_len: usize = "let _ = ".len();

    fn collect_arrows<'a>(
        expr: &Expression<'a>,
        src: &str,
        pl: usize,
        forbidden: &HashSet<String>,
        module_names: &HashSet<String>,
        ctg: &std::collections::HashMap<String, String>,
        ctp: &std::collections::HashMap<String, PrimitiveValue>,
        out: &mut Vec<(usize, usize, String)>,
    ) {
        match expr {
            Expression::ArrowFunctionExpression(arrow) => {
                let s = arrow.span.start as usize;
                let e = arrow.span.end as usize;
                if s < pl || e <= s { return; }
                let ss = s - pl;
                let se = e - pl;
                if se > src.len() || ss >= se { return; }
                let asrc = &src[ss..se];
                let mut params: Vec<String> = Vec::new();
                for p in &arrow.params.items { collect_binding_names(&p.pattern.kind, &mut params); }
                if let Some(r) = &arrow.params.rest { collect_binding_names(&r.argument.kind, &mut params); }
                let bs = (arrow.body.span.start as usize).saturating_sub(s);
                let be = (arrow.body.span.end as usize).saturating_sub(s);
                let body_text = asrc.get(bs..be).unwrap_or(asrc);
                let free = extract_free_variables(body_text, &params);
                let bad = free.iter().any(|v| forbidden.contains(v) && !module_names.contains(v) && !ctg.contains_key(v) && !ctp.contains_key(v));
                if !bad {
                    out.push((ss, se, asrc.to_string()));
                } else {
                    // Recurse into body statements to find inner pure arrows.
                    for st in &arrow.body.statements {
                        collect_stmts(st, src, pl, forbidden, module_names, ctg, ctp, out);
                    }
                }
            }
            Expression::CallExpression(c) => {
                collect_arrows(&c.callee, src, pl, forbidden, module_names, ctg, ctp, out);
                for a in &c.arguments {
                    match a {
                        oxc_ast::ast::Argument::SpreadElement(se) => {
                            collect_arrows(&se.argument, src, pl, forbidden, module_names, ctg, ctp, out);
                        }
                        _ => {
                            // Argument inherits Expression variants via macro, so all
                            // non-SpreadElement arguments share the same memory layout as Expression.
                            #[allow(clippy::transmute_ptr_to_ptr)]
                            let arg_expr: &Expression<'_> = unsafe {
                                &*(a as *const oxc_ast::ast::Argument<'_> as *const Expression<'_>)
                            };
                            collect_arrows(arg_expr, src, pl, forbidden, module_names, ctg, ctp, out);
                        }
                    }
                }
            }
            Expression::AssignmentExpression(a) => collect_arrows(&a.right, src, pl, forbidden, module_names, ctg, ctp, out),
            Expression::SequenceExpression(sq) => { for ex in &sq.expressions { collect_arrows(ex, src, pl, forbidden, module_names, ctg, ctp, out); } }
            Expression::ConditionalExpression(c) => {
                collect_arrows(&c.test, src, pl, forbidden, module_names, ctg, ctp, out);
                collect_arrows(&c.consequent, src, pl, forbidden, module_names, ctg, ctp, out);
                collect_arrows(&c.alternate, src, pl, forbidden, module_names, ctg, ctp, out);
            }
            _ => {}
        }
    }

    fn collect_stmts<'a>(
        stmt: &Statement<'a>, src: &str, pl: usize,
        forbidden: &HashSet<String>, module_names: &HashSet<String>,
        ctg: &std::collections::HashMap<String, String>,
        ctp: &std::collections::HashMap<String, PrimitiveValue>,
        out: &mut Vec<(usize, usize, String)>,
    ) {
        match stmt {
            Statement::ExpressionStatement(es) => collect_arrows(&es.expression, src, pl, forbidden, module_names, ctg, ctp, out),
            Statement::ReturnStatement(r) => { if let Some(a) = &r.argument { collect_arrows(a, src, pl, forbidden, module_names, ctg, ctp, out); } }
            Statement::VariableDeclaration(vd) => { for d in &vd.declarations { if let Some(i) = &d.init { collect_arrows(i, src, pl, forbidden, module_names, ctg, ctp, out); } } }
            Statement::BlockStatement(b) => { for s in &b.body { collect_stmts(s, src, pl, forbidden, module_names, ctg, ctp, out); } }
            Statement::IfStatement(i) => {
                collect_arrows(&i.test, src, pl, forbidden, module_names, ctg, ctp, out);
                collect_stmts(&i.consequent, src, pl, forbidden, module_names, ctg, ctp, out);
                if let Some(a) = &i.alternate { collect_stmts(a, src, pl, forbidden, module_names, ctg, ctp, out); }
            }
            _ => {}
        }
    }

    let mut to_outline: Vec<(usize, usize, String)> = Vec::new();
    if let Some(stmt) = parsed.program.body.first() {
        if let Statement::VariableDeclaration(vd) = stmt {
            if let Some(decl) = vd.declarations.first() {
                if let Some(init) = &decl.init {
                    match init {
                        Expression::ArrowFunctionExpression(arrow) => {
                            for s in &arrow.body.statements {
                                collect_stmts(s, src, prefix_len, forbidden, module_names, const_local_to_global, const_name_to_primitive, &mut to_outline);
                            }
                        }
                        Expression::FunctionExpression(f) => {
                            if let Some(b) = &f.body {
                                for s in &b.statements {
                                    collect_stmts(s, src, prefix_len, forbidden, module_names, const_local_to_global, const_name_to_primitive, &mut to_outline);
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    if to_outline.is_empty() { return src.to_string(); }

    to_outline.sort_by(|a, b| b.0.cmp(&a.0));
    let mut seen: HashSet<usize> = HashSet::new();
    let mut result = src.to_string();

    for (start, end, arrow_src) in to_outline {
        if !seen.insert(start) { continue; }
        let Some(info) = parse_arrow_info(&arrow_src) else { continue };
        let final_params: Vec<String> = info.param_names.iter().map(|p| {
            if outer_param_names.contains(p) || outer_local_names.contains(p.as_str()) { format!("{}_0", p) } else { p.clone() }
        }).collect();
        let fv = extract_free_variables(&info.body_text, &info.param_names);
        let mut body = info.body_text.clone();
        for v in &fv {
            if let Some(g) = const_local_to_global.get(v.as_str()) { body = rename_word(&body, v, g); }
        }
        for v in &fv {
            if let Some(prim) = const_name_to_primitive.get(v.as_str()) { body = rename_word(&body, v, &primitive_to_literal(prim)); }
        }
        for (orig, ren) in info.param_names.iter().zip(final_params.iter()) {
            if orig != ren { body = rename_word(&body, orig, ren); }
        }
        // Babel's generateUidIdentifier skips "_temp1" → sequence is _temp, _temp2, _temp3, ...
        let name = if *temp_counter == 0 { "_temp".to_string() } else { format!("_temp{}", *temp_counter + 1) };
        *temp_counter += 1;
        let ps = final_params.join(", ");
        let rb = if info.is_expr_body && body.starts_with('(') && body.ends_with(')') {
            let inner = &body[1..body.len()-1];
            if inner.chars().fold(0i32, |d,c| match c { '(' => d+1, ')' => d-1, _ => d }) == 0 { inner.to_string() } else { body.clone() }
        } else { body.clone() };
        let decl = if info.is_expr_body {
            format!("function {}({}) {{\n  return {};\n}}", name, ps, rb)
        } else {
            format!("function {}({}) {}", name, ps, body)
        };
        env.outlined_functions.push((name.clone(), decl));
        if end <= result.len() && start < end {
            result = format!("{}{}{}", &result[..start], name, &result[end..]);
        }
    }
    result
}

/// Pre-normalize inner arrow functions in a block body: convert each `=> { return EXPR; }`
/// (single-statement block body with only a return) to `=> EXPR` (expression body).
/// This prevents a bug in codegen's normalize_arrow_expr_body which incorrectly truncates
/// outlined function bodies by finding the first `=>` (an inner arrow) instead of the outer one.
fn normalize_single_return_arrows(text: &str) -> String {
    let mut result = text.to_string();
    let mut search_start: usize = 0;
    loop {
        let Some(rel_pos) = result[search_start..].find("=>") else { break };
        let arrow_pos = search_start + rel_pos;
        let after_arrow_start = arrow_pos + 2;
        if after_arrow_start >= result.len() { break; }

        // Skip whitespace after `=>` to find what follows
        let after_arrow = &result[after_arrow_start..];
        let ws_len = after_arrow.len() - after_arrow.trim_start().len();
        let brace_pos = after_arrow_start + ws_len;

        if result.as_bytes().get(brace_pos) != Some(&b'{') {
            search_start = arrow_pos + 2;
            continue;
        }

        // Look inside the block: skip past `{` and whitespace
        let after_brace = &result[brace_pos + 1..];
        let inner_ws = after_brace.len() - after_brace.trim_start().len();
        let inner_start = brace_pos + 1 + inner_ws;
        if !result[inner_start..].starts_with("return ") {
            search_start = arrow_pos + 2;
            continue;
        }

        // Scan from after "return " to find `EXPR;` followed by `}`
        let ret_expr_start = inner_start + 7; // skip "return "
        let rest = &result[ret_expr_start..];
        let chars: Vec<char> = rest.chars().collect();
        let mut depth = 1i32;
        let mut semi_idx: Option<usize> = None;
        let mut close_idx: Option<usize> = None;
        for (i, &ch) in chars.iter().enumerate() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 { close_idx = Some(i); break; }
                }
                ';' if depth == 1 => {
                    if semi_idx.is_none() { semi_idx = Some(i); }
                }
                _ => {}
            }
        }
        let (semi, close) = match (semi_idx, close_idx) {
            (Some(s), Some(c)) => (s, c),
            _ => { search_start = arrow_pos + 2; continue; }
        };

        // Verify only whitespace between `;` and `}`
        let between: String = chars[semi + 1..close].iter().collect();
        if !between.trim().is_empty() {
            search_start = arrow_pos + 2;
            continue;
        }

        let expr: String = chars[..semi].iter().collect();
        let expr = expr.trim().to_string();

        // Compute byte offset of `}` (close) in `result`
        let close_byte_offset: usize = chars[..=close].iter().map(|c| c.len_utf8()).sum();
        let replace_end = ret_expr_start + close_byte_offset;

        // Replace `{ return EXPR; }` (from brace_pos to replace_end) with ` EXPR`
        let replacement = format!(" {}", expr);
        result.replace_range(brace_pos..replace_end, &replacement);

        // Continue searching from after `=>`
        search_start = arrow_pos + 2;
    }
    result
}
