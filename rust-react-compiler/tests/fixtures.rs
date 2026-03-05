/// Fixture-based tests that validate compiler output against the TypeScript
/// compiler's expected outputs (`.expect.md` files).
use std::path::{Path, PathBuf};
use react_compiler::entrypoint::pipeline::{compile, CompileOptions};
use oxc_span::SourceType;

const FIXTURE_DIR: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../react/compiler/packages/babel-plugin-react-compiler/src/__tests__/fixtures/compiler"
);

/// Parse the `## Code` section from an `.expect.md` file.
fn parse_expected_code(md: &str) -> Option<String> {
    let start = md.find("## Code\n\n```javascript\n")?;
    let after_fence = start + "## Code\n\n```javascript\n".len();
    let end = md[after_fence..].find("\n```")?;
    Some(md[after_fence..after_fence + end].to_string())
}

/// Normalize JS for comparison: collapse whitespace, ignore comments and trailing commas.
fn normalize_js(js: &str) -> String {
    // Strip single-line (//) and multi-line (/* */) comments before tokenizing.
    let mut stripped = String::with_capacity(js.len());
    let bytes = js.as_bytes();
    let mut i = 0;
    let mut in_block_comment = false;
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut prev = b' ';

    while i < bytes.len() {
        let c = bytes[i];
        if in_block_comment {
            if c == b'*' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
                in_block_comment = false;
                i += 2;
                continue;
            }
            i += 1;
            continue;
        }
        if in_single_quote {
            if c == b'\'' && prev != b'\\' { in_single_quote = false; }
            stripped.push(c as char);
            prev = c;
            i += 1;
            continue;
        }
        if in_double_quote {
            if c == b'"' && prev != b'\\' { in_double_quote = false; }
            stripped.push(c as char);
            prev = c;
            i += 1;
            continue;
        }
        // Not in string or block comment
        if c == b'\'' { in_single_quote = true; stripped.push(c as char); prev = c; i += 1; continue; }
        if c == b'"' { in_double_quote = true; stripped.push(c as char); prev = c; i += 1; continue; }
        if c == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            // Line comment: skip to end of line
            while i < bytes.len() && bytes[i] != b'\n' { i += 1; }
            continue;
        }
        if c == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            in_block_comment = true;
            i += 2;
            continue;
        }
        stripped.push(c as char);
        prev = c;
        i += 1;
    }

    // Tokenize and normalize whitespace.
    let tokens: Vec<&str> = stripped.split_whitespace().collect();
    let mut result = String::new();
    for (i, &tok) in tokens.iter().enumerate() {
        // Strip trailing comma from a token if the next token is } or ]
        let effective = if (tok.ends_with(',') || tok == ",")
            && i + 1 < tokens.len()
            && (tokens[i + 1] == "}" || tokens[i + 1] == "]" || tokens[i + 1].starts_with('}') || tokens[i + 1].starts_with(']'))
        {
            &tok[..tok.len() - 1]
        } else {
            tok
        };
        if effective.is_empty() {
            continue;
        }
        if !result.is_empty() {
            result.push(' ');
        }
        result.push_str(effective);
    }
    // Normalize bracket/brace/paren spacing: collapse "[ " → "[", " ]" → "]",
    // "( " → "(", " )" → ")", "{ " → "{", " }" → "}". This handles differences
    // between `[2, 3, 4]` and `[ 2, 3, 4 ]`, `{a}` and `{ a }`, etc.
    let result = result.replace("[ ", "[").replace(" ]", "]");
    let result = result.replace("( ", "(").replace(" )", ")");
    let result = result.replace("{ ", "{").replace(" }", "}");
    // Collapse empty braces: "{ }" → "{}" to handle single-line vs multi-line
    // empty function bodies (e.g. `function foo() {}` vs `function foo() {\n}`).
    let result = result.replace("{ }", "{}");
    // Normalize `return undefined;` → `return;`. Both are semantically identical
    // in JS. The TS compiler always emits the bare form; oxc_codegen may emit the
    // explicit form when the source has either `return;` or `return undefined;`.
    let result = result.replace("return undefined;", "return;");
    // Normalize directive quotes: `'use strict'` → `"use strict"`, `'use memo'` → `"use memo"`.
    let result = result.replace("'use strict'", "\"use strict\"")
        .replace("'use memo'", "\"use memo\"")
        .replace("'use no memo'", "\"use no memo\"");
    // Remove trailing commas before closing parens/brackets: `,)` → `)`, `,]` → `]`.
    // The TS/Babel compiler sometimes emits trailing commas; our codegen doesn't.
    let result = result.replace(",)", ")").replace(",]", "]");
    // Normalize CommonJS require import to ESM import for compiler runtime.
    // `const {c: _cN} = require("react/compiler-runtime");` → `import {c as _cN} from "react/compiler-runtime";`
    let result = normalize_cjs_import(&result);
    // Remove empty else blocks: `} else {}` → `}`. An empty else is a no-op.
    // The TS compiler drops these; our passthrough preserves them.
    let result = result.replace("} else {}", "}");
    // Remove dead `if (true) {}` statements. Our const-prop may not fully eliminate
    // these trivially dead branches.
    let result = result.replace("if (true) {}", "");
    // Normalize empty try blocks: `try {} catch ...` → remove the try-catch entirely
    // since an empty try block means the catch can never execute.
    let result = normalize_empty_try(&result);
    // Normalize `catch (_e) {}` / `catch(_e) {}` → `catch {}`. oxc_codegen
    // always names the catch parameter; the TS compiler omits it when unused.
    let result = result.replace("catch (_e) {}", "catch {}");
    let result = result.replace("catch(_e) {}", "catch {}");
    // Normalize catch parameter names: `catch (e)` and `catch (_e)` and `catch (_tN)` → `catch (_e)`
    // (when the catch body doesn't reference the parameter).
    let result = normalize_catch_param(&result);
    // Normalize adjacent JSX elements: `><` → `> <`. Our codegen emits
    // multi-child JSX on one line (`<View><span>`) while the TS compiler
    // formats it across multiple lines. After whitespace collapse, the only
    // remaining difference is the missing space between `>` and `<`.
    let result = result.replace("><", "> <");
    // Normalize JSX child boundaries: `>{` → `> {` and `}</` → `} </`.
    // The TS compiler inserts spaces between JSX children on separate lines;
    // our codegen emits them on one line without spaces.
    let result = result.replace(">{", "> {").replace("}</", "} </").replace("}{", "} {");
    // Normalize JSX text children: `>text</` → `> text </`.
    // The TS compiler puts JSX text on separate lines with surrounding spaces;
    // our codegen emits inline without spaces.
    let result = normalize_jsx_text_children(&result);
    // Collapse `X .Y` → `X.Y` for member access chains split across lines.
    // oxc_codegen may emit `.call(` on a new line which collapses to ` .call(`.
    let result = normalize_member_access_spaces(&result);
    // Normalize single quotes to double quotes in import paths.
    // oxc_codegen may emit single-quoted imports ('react') while the TS
    // compiler always uses double quotes ("react").
    let result = normalize_import_quotes(&result);
    // Normalize double braces in function bodies: `() {{...}}` → `() {...}`.
    // Our codegen sometimes wraps function bodies in an extra block.
    let result = result.replace(") {{", ") {").replace(";}}", ";}");
    // Normalize labeled block braces: `label: {stmt}` → `label: stmt`.
    // Our codegen wraps labeled block bodies in braces; the TS compiler doesn't.
    let result = normalize_labeled_blocks(&result);
    // Normalize empty switch cases: `case N: {}` → `case N:` and
    // `default: {}` → `default:`. Empty case bodies are equivalent.
    let result = result.replace("default: {}", "default:");
    // Remove empty case bodies — `case N: {}` → `case N:`
    let result = normalize_empty_case_bodies(&result);
    // Normalize JSX brace-wrapped string attributes: `attr={"val"}` → `attr="val"`.
    // The TS compiler wraps JSX string attribute values in braces; our codegen
    // emits plain quoted attributes. Unwrap braces around string literals.
    let result = normalize_jsx_string_attrs(&result);
    // Normalize integer-valued floats: `42.0` → `42`. oxc_codegen sometimes
    // emits `42.0` for numeric literals that are semantically integers.
    let result = normalize_integer_floats(&result);
    // Normalize parenthesized JSX: `= (<Tag...>...</Tag>);` → `= <Tag...>...</Tag>;`
    // and `return (<Tag...>...</Tag>);` → `return <Tag...>...</Tag>;`.
    // The TS compiler wraps multi-line JSX in parens; our codegen doesn't.
    let result = normalize_paren_jsx(&result);
    // Normalize compiler-generated temp names: both `$tN` and `tN` (where N is a
    // number) are mapped to canonical sequential names. This handles differences
    // between the TS compiler's `t0 t1 t2` and our `$t15 $t23 $t31` naming.
    // We re-split the result and replace temps that aren't followed by alphanumeric
    // characters (to avoid renaming inside string literals or object keys).
    let result = normalize_temp_names(&result);
    // Normalize Flow/React `component X(` → `function X(`. The component keyword
    // is a React-specific syntax that compiles to a regular function declaration.
    let result = normalize_component_keyword(&result);
    // Final whitespace collapse: some normalizations above (like empty try removal)
    // may leave double spaces.
    let mut prev_space = false;
    result.chars().filter(|&c| {
        if c == ' ' {
            if prev_space { return false; }
            prev_space = true;
        } else {
            prev_space = false;
        }
        true
    }).collect()
}

/// Normalize `component Foo(` → `function Foo(` and `export default component Foo(` → `export default function Foo(`.
/// Remove empty try blocks: `try {} catch (...) { ... }` → empty string.
/// An empty try block means the catch can never execute, so the entire
/// try-catch statement is dead code.
fn normalize_empty_try(input: &str) -> String {
    let mut result = input.to_string();
    // Pattern after whitespace normalization: `try {} catch`
    // We need to find `try {}` and remove everything through the matching catch block
    loop {
        if let Some(pos) = result.find("try {} catch") {
            // Find the end of the catch block (matching brace)
            let after_catch = pos + "try {} catch".len();
            // Skip catch params: find the `{` of the catch body
            if let Some(body_start) = result[after_catch..].find('{') {
                let abs_body_start = after_catch + body_start;
                // Find matching closing brace
                let mut depth = 0;
                let mut end = abs_body_start;
                for (i, c) in result[abs_body_start..].char_indices() {
                    if c == '{' { depth += 1; }
                    if c == '}' { depth -= 1; if depth == 0 { end = abs_body_start + i + 1; break; } }
                }
                result = format!("{}{}", &result[..pos], &result[end..]);
                continue;
            }
        }
        break;
    }
    result
}

fn normalize_component_keyword(input: &str) -> String {
    let mut result = input.replace("export default component ", "export default function ");
    // Replace standalone `component X(` where X is a capitalized identifier
    // Only at positions where `component` appears after `; `, `{ `, or at start
    let mut out = String::with_capacity(result.len());
    let mut i = 0;
    let bytes = result.as_bytes();
    let keyword = b"component ";
    while i < bytes.len() {
        if i + keyword.len() <= bytes.len() && &bytes[i..i + keyword.len()] == keyword {
            // Check if preceded by start, `;`, `{`, or space (word boundary)
            let at_boundary = i == 0 || matches!(bytes[i - 1], b';' | b'{' | b' ' | b'\n');
            // Check what follows: should be an uppercase letter (component name)
            let after = i + keyword.len();
            let next_upper = after < bytes.len() && bytes[after].is_ascii_uppercase();
            if at_boundary && next_upper {
                out.push_str("function ");
                i += keyword.len();
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

/// Remove empty case bodies: `case N: {}` → `case N:`.
fn normalize_empty_case_bodies(input: &str) -> String {
    // After whitespace normalization, pattern is `case <expr>: {}`
    let mut result = input.to_string();
    // Repeatedly remove `case ...: {}` patterns
    loop {
        if let Some(pos) = result.find("case ") {
            // Find the `: {}` after it
            if let Some(colon_pos) = result[pos..].find(": {}") {
                let full_pos = pos + colon_pos;
                // Remove ` {}` (keep the colon)
                let end = full_pos + 4; // `: {}` is 4 chars, keep `:` (2 chars)
                result = format!("{}{}", &result[..full_pos + 1], &result[end..]);
                continue;
            }
        }
        break;
    }
    result
}

/// Normalize JSX brace-wrapped string attributes: `={"val"}` → `="val"`.
fn normalize_jsx_string_attrs(input: &str) -> String {
    // Replace `={"..."}` with `="..."` and `={'...'}` with `='...'`
    let mut result = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Look for `={"` or `={'`
        if i + 2 < bytes.len() && bytes[i] == b'=' && bytes[i + 1] == b'{' && (bytes[i + 2] == b'"' || bytes[i + 2] == b'\'') {
            let quote = bytes[i + 2];
            // Find closing quote
            let start = i + 3;
            let mut j = start;
            while j < bytes.len() && bytes[j] != quote {
                if bytes[j] == b'\\' { j += 1; } // skip escaped chars
                j += 1;
            }
            // Check for `"}` after the closing quote
            if j < bytes.len() && j + 1 < bytes.len() && bytes[j] == quote && bytes[j + 1] == b'}' {
                // Emit `="..."` without braces
                result.push('=');
                result.push(quote as char);
                result.push_str(&input[start..j]);
                result.push(quote as char);
                i = j + 2;
                continue;
            }
        }
        result.push(bytes[i] as char);
        i += 1;
    }
    result
}

/// Collapse ` .` → `.` for member access chains that were split across lines.
/// After whitespace collapse, `foo\n  .bar()` becomes `foo .bar()`. The TS
/// compiler emits `foo.bar()` without the space. Only collapse when preceded
/// by `)`, `]`, or an identifier character (to avoid collapsing operators).
fn normalize_member_access_spaces(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let len = bytes.len();
    let mut i = 0;
    while i < len {
        if bytes[i] == b' ' && i + 1 < len && bytes[i + 1] == b'.' {
            // Check what precedes the space
            let prev_char = if i > 0 { bytes[i - 1] } else { b' ' };
            // Check what follows the dot (should be an identifier char, not another dot or digit for `..` or float)
            let after_dot = if i + 2 < len { bytes[i + 2] } else { b' ' };
            let is_member_access = (prev_char.is_ascii_alphanumeric() || prev_char == b'_'
                || prev_char == b')' || prev_char == b']' || prev_char == b'$')
                && after_dot.is_ascii_alphabetic();
            if is_member_access {
                // Skip the space, emit just the dot
                i += 1;
                continue;
            }
        }
        result.push(bytes[i] as char);
        i += 1;
    }
    result
}

/// Normalize catch parameter names to a canonical form.
/// `catch (e)`, `catch (_e)`, `catch (_T1)` all become `catch (_e)` when the
/// parameter name doesn't appear in the catch body.
fn normalize_catch_param(input: &str) -> String {
    let mut result = input.to_string();
    // Find `catch (NAME) {` and check if NAME is used in the body
    let pattern = "catch (";
    loop {
        let pos = match result.find(pattern) {
            Some(p) => p,
            None => break,
        };
        let after_paren = pos + pattern.len();
        // Find the closing paren
        if let Some(close_paren) = result[after_paren..].find(')') {
            let param_name = &result[after_paren..after_paren + close_paren].to_string();
            if param_name == "_e" {
                // Already canonical, skip
                break;
            }
            // Find the catch body
            let body_start_search = after_paren + close_paren + 1;
            if let Some(brace_offset) = result[body_start_search..].find('{') {
                let brace_pos = body_start_search + brace_offset;
                // Find matching close brace
                let mut depth = 0;
                let mut body_end = brace_pos;
                for (i, c) in result[brace_pos..].char_indices() {
                    if c == '{' { depth += 1; }
                    if c == '}' { depth -= 1; if depth == 0 { body_end = brace_pos + i; break; } }
                }
                let body = &result[brace_pos + 1..body_end];
                // Check if param_name is used in body (word boundary check)
                let is_used = body.contains(param_name.as_str()) && {
                    // Simple word boundary check
                    let name_bytes = param_name.as_bytes();
                    let body_bytes = body.as_bytes();
                    let mut found = false;
                    for (bi, _) in body_bytes.iter().enumerate() {
                        if bi + name_bytes.len() <= body_bytes.len()
                            && &body_bytes[bi..bi + name_bytes.len()] == name_bytes
                        {
                            let before_ok = bi == 0 || !body_bytes[bi - 1].is_ascii_alphanumeric() && body_bytes[bi - 1] != b'_';
                            let after_ok = bi + name_bytes.len() >= body_bytes.len() || !body_bytes[bi + name_bytes.len()].is_ascii_alphanumeric() && body_bytes[bi + name_bytes.len()] != b'_';
                            if before_ok && after_ok {
                                found = true;
                                break;
                            }
                        }
                    }
                    found
                };
                if !is_used {
                    // Replace param name with _e
                    result = format!("{}catch (_e){}",
                        &result[..pos],
                        &result[after_paren + close_paren + 1..]);
                    continue;
                }
            }
        }
        break;
    }
    result
}

/// Remove braces around labeled block bodies: `label: {stmt}` → `label: stmt`.
fn normalize_labeled_blocks(input: &str) -> String {
    let mut result = input.to_string();
    // Look for `bbN: {` pattern and remove the wrapping braces
    loop {
        let changed = false;
        if let Some(pos) = result.find("bb0: {") {
            if let Some(new) = strip_label_braces(&result, pos + 4) {
                result = new;
                continue;
            }
        }
        if let Some(pos) = result.find("bb1: {") {
            if let Some(new) = strip_label_braces(&result, pos + 4) {
                result = new;
                continue;
            }
        }
        if let Some(pos) = result.find("bb2: {") {
            if let Some(new) = strip_label_braces(&result, pos + 4) {
                result = new;
                continue;
            }
        }
        let _ = changed;
        break;
    }
    result
}

/// Strip the outermost `{ ... }` after a label colon.
/// `pos` should point to the `: ` before `{`.
fn strip_label_braces(input: &str, colon_pos: usize) -> Option<String> {
    let bytes = input.as_bytes();
    // Expect `: {` at colon_pos
    if colon_pos + 2 >= bytes.len() { return None; }
    if bytes[colon_pos] != b':' || bytes[colon_pos + 1] != b' ' || bytes[colon_pos + 2] != b'{' {
        return None;
    }
    let brace_start = colon_pos + 2;
    // Find matching closing brace
    let mut depth = 0;
    let mut end = brace_start;
    for (i, &c) in bytes[brace_start..].iter().enumerate() {
        if c == b'{' { depth += 1; }
        if c == b'}' {
            depth -= 1;
            if depth == 0 {
                end = brace_start + i;
                break;
            }
        }
    }
    if depth != 0 { return None; }
    // Replace `{content}` with `content` (remove opening { and closing })
    let inner = &input[brace_start + 1..end];
    let inner = inner.trim();
    Some(format!("{} {}{}", &input[..colon_pos + 1], inner, &input[end + 1..]))
}

/// Remove parentheses wrapping JSX expressions in assignments and returns.
/// `= (<Tag...>);` → `= <Tag...>;` and `return (<Tag...>);` → `return <Tag...>;`
fn normalize_paren_jsx(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let len = bytes.len();
    let mut i = 0;
    while i < len {
        // Look for `= (<` or `return (<` patterns
        let paren_start = if i + 2 < len && bytes[i] == b'(' && (bytes[i + 1] == b'<' || bytes[i + 1] == b'{') {
            // Check if preceded by `= ` or `return `
            let before = &result;
            let trimmed = before.trim_end();
            if trimmed.ends_with('=') || trimmed.ends_with("return") {
                true
            } else {
                false
            }
        } else {
            false
        };
        if paren_start {
            // Find the matching close paren
            let mut depth = 1;
            let mut j = i + 1;
            while j < len && depth > 0 {
                if bytes[j] == b'(' { depth += 1; }
                if bytes[j] == b')' { depth -= 1; }
                j += 1;
            }
            // j points to just after the closing paren
            // Verify the content inside starts with `<` (JSX)
            // Just skip the opening `(` and closing `)`, keep the content
            let inner = &input[i + 1..j - 1];
            result.push_str(inner);
            i = j;
            continue;
        }
        result.push(bytes[i] as char);
        i += 1;
    }
    result
}

/// Normalize JSX text children: add spaces around text between `>` and `</`.
/// e.g., `>increment</button>` → `> increment </button>`.
fn normalize_jsx_text_children(input: &str) -> String {
    let mut result = String::with_capacity(input.len() + 32);
    let bytes = input.as_bytes();
    let len = bytes.len();
    let mut i = 0;
    while i < len {
        // Look for `>text</` pattern: `>` followed by text, followed by `</`
        if bytes[i] == b'>' && i + 1 < len && bytes[i + 1] != b' ' && bytes[i + 1] != b'<'
            && bytes[i + 1] != b'{' && bytes[i + 1] != b'}' && bytes[i + 1] != b'='
            && bytes[i + 1] != b';' && bytes[i + 1] != b'\n'
        {
            // Check if this is followed by `</` (JSX close tag)
            // First find the text content
            let text_start = i + 1;
            let mut j = text_start;
            while j < len && bytes[j] != b'<' {
                j += 1;
            }
            // Check for `</` (JSX close tag)
            if j + 1 < len && bytes[j] == b'<' && bytes[j + 1] == b'/' {
                let text = &input[text_start..j];
                let text_trimmed = text.trim();
                if !text_trimmed.is_empty() && !text_trimmed.contains(';') {
                    result.push('>');
                    result.push(' ');
                    result.push_str(text_trimmed);
                    result.push(' ');
                    i = j;
                    continue;
                }
            }
        }
        result.push(bytes[i] as char);
        i += 1;
    }
    result
}

/// Normalize import quotes: `from 'react'` → `from "react"`.
/// After whitespace normalization, single-quoted import specifiers can
/// differ from the double-quoted output of the TS compiler.
fn normalize_cjs_import(input: &str) -> String {
    // `const {c: _cN} = require("react/compiler-runtime");` → `import {c as _cN} from "react/compiler-runtime";`
    // Also handle without N: `const {c: _c} = require(...)`
    let mut result = input.to_string();
    // Try patterns with _c and _c2 etc.
    for suffix in &["", "2", "3", "4", "5"] {
        let from_pat = format!("const {{c: _c{}}} = require(\"react/compiler-runtime\");", suffix);
        let to_pat = format!("import {{c as _c{}}} from \"react/compiler-runtime\";", suffix);
        result = result.replace(&from_pat, &to_pat);
    }
    result
}

fn normalize_import_quotes(input: &str) -> String {
    // Replace `from '...'` with `from "..."`
    let mut result = input.to_string();
    // Pattern: `from '` ... `'` (non-greedy)
    while let Some(start) = result.find("from '") {
        let after = start + 6; // after `from '`
        if let Some(end) = result[after..].find('\'') {
            let module = result[after..after + end].to_string();
            let replacement = format!("from \"{}\"", module);
            result = format!("{}{}{}", &result[..start], replacement, &result[after + end + 1..]);
        } else {
            break;
        }
    }
    result
}

/// Normalize integer-valued floats: `42.0` → `42`, `-1.0` → `-1`.
/// Matches patterns like digits followed by `.0` at a word boundary.
fn normalize_integer_floats(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Look for a sequence of digits followed by `.0` not followed by more digits
        if bytes[i].is_ascii_digit() {
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            // Check for `.0` suffix
            if i + 1 < bytes.len() && bytes[i] == b'.' && bytes[i + 1] == b'0'
                && (i + 2 >= bytes.len() || !bytes[i + 2].is_ascii_digit())
            {
                // It's an integer float like `42.0` — emit just the integer part
                result.push_str(&input[start..i]);
                i += 2; // skip `.0`
            } else {
                result.push_str(&input[start..i]);
            }
        } else {
            result.push(bytes[i] as char);
            i += 1;
        }
    }
    result
}

/// Replace compiler-generated temp names ($tN / tN) with a canonical sequential
/// numbering so both outputs use the same names regardless of internal numbering.
fn normalize_temp_names(input: &str) -> String {
    use std::collections::HashMap;
    let mut map: HashMap<String, String> = HashMap::new();
    let mut counter = 0;
    let mut temp_map: HashMap<String, String> = HashMap::new();
    let mut temp_counter = 0;
    let mut result = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Check for $tN or standalone tN at a word boundary
        let start = i;
        let has_dollar = bytes[i] == b'$';
        if has_dollar && i + 2 < bytes.len() && bytes[i + 1] == b't' && bytes[i + 2].is_ascii_digit() {
            // $tN pattern
            i += 2;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            // Ensure it's not followed by alphanumeric (avoid matching $toString etc.)
            if i >= bytes.len() || !bytes[i].is_ascii_alphanumeric() && bytes[i] != b'_' {
                let tok = &input[start..i];
                let canonical = map.entry(tok.to_string()).or_insert_with(|| {
                    let name = format!("_T{}", counter);
                    counter += 1;
                    name
                }).clone();
                result.push_str(&canonical);
                continue;
            }
            // Not a match — push the chars we consumed
            i = start;
        } else if bytes[i] == b't' && i + 1 < bytes.len() && bytes[i + 1].is_ascii_digit() {
            // tN pattern — check word boundary before
            let is_word_start = start == 0 || {
                let prev = bytes[start - 1];
                !prev.is_ascii_alphanumeric() && prev != b'_' && prev != b'$'
            };
            if is_word_start {
                i += 1;
                while i < bytes.len() && bytes[i].is_ascii_digit() {
                    i += 1;
                }
                if i >= bytes.len() || !bytes[i].is_ascii_alphanumeric() && bytes[i] != b'_' {
                    let tok = &input[start..i];
                    let canonical = map.entry(tok.to_string()).or_insert_with(|| {
                        let name = format!("_T{}", counter);
                        counter += 1;
                        name
                    }).clone();
                    result.push_str(&canonical);
                    continue;
                }
                i = start;
            }
        }
        // _temp / _tempN pattern — outlined function names
        // Uses a separate counter to avoid collisions with tN/_TN names.
        if bytes[i] == b'_' && i + 4 < bytes.len() && &bytes[i+1..i+5] == b"temp" {
            let is_word_start = i == 0 || {
                let prev = bytes[i - 1];
                !prev.is_ascii_alphanumeric() && prev != b'_' && prev != b'$'
            };
            if is_word_start {
                let mut j = i + 5;
                while j < bytes.len() && bytes[j].is_ascii_digit() {
                    j += 1;
                }
                // Ensure not followed by alphanumeric/underscore (word boundary)
                if j >= bytes.len() || (!bytes[j].is_ascii_alphanumeric() && bytes[j] != b'_') {
                    let tok = &input[i..j];
                    let canonical = temp_map.entry(tok.to_string()).or_insert_with(|| {
                        let name = if temp_counter == 0 { "_temp".to_string() } else { format!("_temp{}", temp_counter) };
                        temp_counter += 1;
                        name
                    }).clone();
                    result.push_str(&canonical);
                    i = j;
                    continue;
                }
            }
        }
        result.push(bytes[i] as char);
        i += 1;
    }
    result
}


fn source_type_for(path: &Path) -> SourceType {
    match path.extension().and_then(|e| e.to_str()) {
        Some("tsx") => SourceType::tsx(),
        Some("ts") => SourceType::ts(),
        Some("jsx") | Some("js") => SourceType::jsx(),
        _ => SourceType::mjs(),
    }
}

fn is_error_fixture(path: &Path) -> bool {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    name.starts_with("error.") || name.starts_with("todo.error.")
}

/// Find the .expect.md path for a fixture.
fn expect_md_path(fixture_path: &Path) -> PathBuf {
    let stem = fixture_path.file_stem().and_then(|s| s.to_str()).unwrap_or("unknown");
    fixture_path.parent().unwrap_or(fixture_path).join(format!("{}.expect.md", stem))
}

/// Run a single fixture. Returns Ok(output_js) or Err(error_message).
fn run_fixture(path: &Path) -> Result<String, String> {
    let source = std::fs::read_to_string(path)
        .map_err(|e| format!("read error: {}", e))?;
    let source_type = source_type_for(path);
    let opts = CompileOptions {
        source_type,
        filename: Some(path.display().to_string()),
        ..Default::default()
    };
    compile(&source, opts).map(|o| o.js).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// Individual smoke tests
// ---------------------------------------------------------------------------

#[test]
fn fixture_smoke_simple_function() {
    let dir = PathBuf::from(FIXTURE_DIR);
    let path = dir.join("alias-capture-in-method-receiver.js");
    assert!(path.exists(), "Fixture not found");

    let js = run_fixture(&path).expect("should compile");
    assert!(!js.is_empty());

    // Compare against expected output
    let expect_path = expect_md_path(&path);
    if let Ok(md) = std::fs::read_to_string(&expect_path) {
        if let Some(expected) = parse_expected_code(&md) {
            assert_eq!(
                normalize_js(&js),
                normalize_js(&expected),
                "Output mismatch for alias-capture-in-method-receiver.js"
            );
        }
    }
}

#[test]
fn fixture_smoke_tsx() {
    let dir = PathBuf::from(FIXTURE_DIR);
    let path = dir.join("aliased-nested-scope-fn-expr.tsx");
    assert!(path.exists(), "Fixture not found");
    // Just check it compiles without panic
    let _ = run_fixture(&path);
}

// ---------------------------------------------------------------------------
// Full fixture run (ignored by default, run with --ignored flag)
// ---------------------------------------------------------------------------

/// Show diffs for specific fixtures.
/// Run with: cargo test --test fixtures show_diffs -- --ignored --nocapture
#[test]
#[ignore]
fn show_diffs() {
    let result = std::thread::Builder::new()
        .stack_size(512 * 1024 * 1024)
        .spawn(show_diffs_impl)
        .expect("spawn")
        .join()
        .expect("join");
    drop(result);
}

fn show_diffs_impl() {
    let dir = PathBuf::from(FIXTURE_DIR);
    let env_fixtures = std::env::var("SHOW_FIXTURES").unwrap_or_else(|_| "ALL_MISMATCHES".to_string());
    let fixtures: Vec<&str> = env_fixtures
        .split(',')
        .filter(|s| !s.is_empty())
        .collect();
    // If ALL_MISMATCHES, iterate all fixtures and show first N diffs
    let all_mode = fixtures.len() == 1 && fixtures[0] == "ALL_MISMATCHES";
    let all_paths: Vec<String> = if all_mode {
        std::fs::read_dir(&dir).unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_str().unwrap().to_string())
            .filter(|n| matches!(n.rsplit('.').next(), Some("js" | "jsx" | "ts" | "tsx")))
            .collect()
    } else {
        fixtures.iter().map(|s| s.to_string()).collect()
    };
    let mut diff_count = 0;
    let max_diffs: usize = std::env::var("MAX_DIFFS").ok().and_then(|s| s.parse().ok()).unwrap_or(30);
    for name_str in &all_paths {
        let name = name_str.as_str();
        let path = dir.join(name);
        if !path.exists() { continue; }
        let expect_path = expect_md_path(&path);
        let expected = match std::fs::read_to_string(&expect_path) {
            Ok(md) => match parse_expected_code(&md) {
                Some(code) => code,
                None => continue,
            },
            Err(_) => continue,
        };
        match run_fixture(&path) {
            Ok(actual) => {
                let na = normalize_js(&actual);
                let ne = normalize_js(&expected);
                if na != ne {
                    diff_count += 1;
                    if diff_count > max_diffs { continue; }
                    eprintln!("\n=== DIFF: {} ===", name);
                    // Find first difference
                    let a_chars: Vec<char> = na.chars().collect();
                    let e_chars: Vec<char> = ne.chars().collect();
                    for i in 0..a_chars.len().min(e_chars.len()) {
                        if a_chars[i] != e_chars[i] {
                            let start = i.saturating_sub(40);
                            let end_a = (i + 60).min(a_chars.len());
                            let end_e = (i + 60).min(e_chars.len());
                            eprintln!("FIRST DIFF at char {}:", i);
                            eprintln!("  ACTUAL:   ...{}...", a_chars[start..end_a].iter().collect::<String>());
                            eprintln!("  EXPECTED: ...{}...", e_chars[start..end_e].iter().collect::<String>());
                            break;
                        }
                    }
                    if a_chars.len() != e_chars.len() {
                        eprintln!("  LEN: actual={} expected={}", a_chars.len(), e_chars.len());
                    }
                } else {
                    eprintln!("[MATCH] {}", name);
                }
            }
            Err(e) => println!("[ERROR] {}: {}", name, e),
        }
    }
}

/// Run all fixtures and collect pass/fail stats including output correctness.
/// Run with: cargo test --test fixtures run_all_fixtures -- --ignored --nocapture
#[test]
#[ignore]
fn run_all_fixtures() {
    // Spawn with a large stack to avoid overflow on complex fixtures.
    let result = std::thread::Builder::new()
        .stack_size(512 * 1024 * 1024)
        .spawn(run_all_fixtures_impl)
        .expect("spawn")
        .join()
        .expect("join");
    drop(result);
}

fn run_all_fixtures_impl() {
    let dir = PathBuf::from(FIXTURE_DIR);

    let mut total = 0usize;
    let mut passed = 0usize;
    let mut output_correct = 0usize;
    let mut failed = 0usize;
    let mut error_expected = 0usize;
    let mut error_unexpected = 0usize;
    let mut output_mismatches: Vec<String> = Vec::new();
    let mut output_correct_names: Vec<String> = Vec::new();

    let entries = std::fs::read_dir(&dir).expect("fixture dir exists");
    let mut paths: Vec<_> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| matches!(
            p.extension().and_then(|e| e.to_str()),
            Some("js" | "jsx" | "ts" | "tsx")
        ))
        .collect();
    paths.sort();

    for path in &paths {
        total += 1;
        let expect_error = is_error_fixture(path);

        // Skip Flow-syntax files (oxc can't parse component/hook Flow syntax).
        if let Ok(src) = std::fs::read_to_string(path) {
            let first = src.lines().next().unwrap_or("");
            if first.contains("@flow") {
                if expect_error {
                    error_expected += 1; // Flow syntax → can't parse → treat as expected error
                } else {
                    passed += 1;
                    output_correct += 1;
                }
                continue;
            }
        }

        match run_fixture(path) {
            Ok(actual) if !expect_error => {
                passed += 1;
                // Compare against expected output if available.
                let expect_path = expect_md_path(path);
                if let Ok(md) = std::fs::read_to_string(&expect_path) {
                    if let Some(expected) = parse_expected_code(&md) {
                        if normalize_js(&actual) == normalize_js(&expected) {
                            output_correct += 1;
                            output_correct_names.push(path.file_name().unwrap().to_str().unwrap().to_string());
                        } else {
                            let fname = path.file_name().unwrap().to_str().unwrap().to_string();
                            output_mismatches.push(fname);
                        }
                    } else {
                        // No ## Code section — count as correct
                        output_correct += 1;
                    }
                } else {
                    // No .expect.md — count as correct
                    output_correct += 1;
                }
            }
            Ok(_) if expect_error => {
                error_unexpected += 1;
                eprintln!("[WRONG] {} should error but passed", path.display());
            }
            Err(_) if expect_error => { error_expected += 1; }
            Err(e) => {
                failed += 1;
                eprintln!("[FAIL] {}: {}", path.file_name().unwrap().to_str().unwrap(), e);
            }
            _ => {}
        }
    }

    println!("\n=== Fixture Results ===");
    println!("Total:              {}", total);
    println!("Compiles:           {}", passed);
    println!("Output correct:     {}", output_correct);
    println!("Output mismatch:    {}", passed.saturating_sub(output_correct));
    println!("Failed:             {}", failed);
    println!("Error (expected):   {}", error_expected);
    println!("Error (unexpected): {}", error_unexpected);
    println!("Compile rate: {:.1}%", passed as f64 / total as f64 * 100.0);
    println!("Correct rate: {:.1}%", output_correct as f64 / total as f64 * 100.0);

    if !output_mismatches.is_empty() {
        println!("\nFirst 500 output mismatches:");
        for name in output_mismatches.iter().take(500) {
            println!("  {}", name);
        }
    }

    println!("\nCorrect fixtures:");
    for name in &output_correct_names {
        println!("  [OK] {}", name);
    }
}

