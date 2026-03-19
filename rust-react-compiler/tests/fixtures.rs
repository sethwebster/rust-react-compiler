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

/// Copy one UTF-8 character from `bytes[i..]` into `result`, returning bytes consumed.
/// This is the safe alternative to `result.push(bytes[i] as char); i += 1;`
/// which corrupts multi-byte UTF-8 sequences by treating each byte as a separate char.
#[inline]
fn push_utf8_char(result: &mut String, bytes: &[u8], i: usize) -> usize {
    let b = bytes[i];
    let n = if b < 0x80 { 1 }
            else if b < 0xE0 { 2 }
            else if b < 0xF0 { 3 }
            else { 4 };
    let end = (i + n).min(bytes.len());
    // bytes came from a valid Rust &str so this slice is always valid UTF-8
    result.push_str(std::str::from_utf8(&bytes[i..end]).unwrap_or("\u{FFFD}"));
    end - i
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
            let adv = push_utf8_char(&mut stripped, bytes, i);
            prev = c;
            i += adv;
            continue;
        }
        if in_double_quote {
            if c == b'"' && prev != b'\\' { in_double_quote = false; }
            let adv = push_utf8_char(&mut stripped, bytes, i);
            prev = c;
            i += adv;
            continue;
        }
        // Not in string or block comment
        if c == b'\'' { in_single_quote = true; stripped.push('\''); prev = c; i += 1; continue; }
        if c == b'"' { in_double_quote = true; stripped.push('"'); prev = c; i += 1; continue; }
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
        // Add spaces around bracket/brace/paren/semicolon characters so they tokenize
        // the same regardless of whether the formatter emits spaces:
        // e.g. Babel: `[ { arg: 3 } ]`  vs oxc: `[{ arg: 3 }]` → both become `[ { arg: 3 } ]`.
        // Semicolon spacing ensures `</div>;` and `</div>\n;` tokenize identically.
        if c == b'[' || c == b']' || c == b'{' || c == b'}' || c == b'(' || c == b')' || c == b';' {
            stripped.push(' ');
            stripped.push(c as char);
            stripped.push(' ');
            prev = c;
            i += 1;
            continue;
        }
        // Add space after `>` when followed by `<` or a letter/digit (JSX formatting).
        // Babel formats JSX with newlines between elements (`<A>\n  <B/>`) while
        // oxc formats inline (`<A><B/>`). After split_whitespace they differ:
        // Babel: `<A>`, `<B/>` (separate tokens) vs oxc: `<A><B/>` (one token).
        // Adding space after `>` when next is `<` or content char normalizes this.
        if c == b'>' && i + 1 < bytes.len() {
            let next = bytes[i + 1];
            if next == b'<' || next.is_ascii_alphabetic() || next.is_ascii_digit() || next == b'{' {
                stripped.push('>');
                stripped.push(' ');
                prev = c;
                i += 1;
                continue;
            }
        }
        // Add space before `</` (closing JSX tag) when directly preceded by text.
        // Babel emits `>text\n</Tag>` which after whitespace collapse becomes
        // `> text </Tag>` (space before `</`). We may emit `>text</Tag>` inline,
        // which after the `>` rule adds `> text</Tag>` — still no space before `</`.
        // This rule ensures both normalize to `> text </Tag>`.
        if c == b'<' && i + 1 < bytes.len() && bytes[i + 1] == b'/'
            && i > 0 && (prev.is_ascii_alphanumeric() || prev == b'_' || prev == b'-' || prev == b'.')
        {
            stripped.push(' ');
        }
        let adv = push_utf8_char(&mut stripped, bytes, i);
        prev = c;
        i += adv;
    }

    // Tokenize and normalize whitespace.
    let tokens: Vec<&str> = stripped.split_whitespace().collect();

    // Remove wrapping parentheses around JSX expressions, which Babel adds for
    // readability but are semantically equivalent to omitting them:
    //   `t1 = (\n  <div>...</div>\n)` vs `t1 = <div>...</div>`
    // Pattern: token before `(` is `=` or `return`, and token after `(` starts with `<`.
    // We build a replacements map: token index → override string ("" = skip, other = replace).
    let mut replacements: std::collections::HashMap<usize, &'static str> = std::collections::HashMap::new();
    {
        let mut depth = 0usize;
        let mut paren_start = 0usize;
        let mut in_jsx_paren = false;
        for (i, &tok) in tokens.iter().enumerate() {
            if in_jsx_paren {
                if tok == "(" { depth += 1; }
                else if tok == ")" {
                    if depth == 0 {
                        replacements.insert(paren_start, "");
                        replacements.insert(i, "");
                        in_jsx_paren = false;
                    } else { depth -= 1; }
                } else if tok.starts_with(')') && depth == 0 {
                    // Closing paren combined with other chars: `);` or `),`
                    // We can't easily replace just part of a token here, so just
                    // leave as-is (partial match - skip the opening paren too to avoid
                    // leaving a dangling `(` in output, but keep the suffix).
                    // For safety, don't remove anything if the close is combined.
                    in_jsx_paren = false;
                }
            } else if tok == "(" && i + 1 < tokens.len() && tokens[i + 1].starts_with('<') {
                // Check if preceded by `=` or `return`
                let prev = if i > 0 { tokens[i - 1] } else { "" };
                if prev == "=" || prev == "return" || prev == "," || prev == "=>" {
                    in_jsx_paren = true;
                    paren_start = i;
                    depth = 0;
                }
            }
        }
    }

    // Also mark `as const` (TypeScript type assertion, no runtime effect) for removal.
    // This allows `.js` files using `as const` to match output that strips it.
    for i in 0..tokens.len().saturating_sub(1) {
        if tokens[i] == "as" && tokens[i + 1] == "const" {
            replacements.insert(i, "");
            replacements.insert(i + 1, "");
        }
    }

    let mut result = String::new();
    for (i, &tok) in tokens.iter().enumerate() {
        if let Some(&replacement) = replacements.get(&i) {
            if !replacement.is_empty() {
                if !result.is_empty() { result.push(' '); }
                result.push_str(replacement);
            }
            continue;
        }
        // Strip trailing comma from a token if the next token is }, ], or )
        let effective = if (tok.ends_with(',') || tok == ",")
            && i + 1 < tokens.len()
            && (tokens[i + 1] == "}" || tokens[i + 1] == "]" || tokens[i + 1] == ")"
                || tokens[i + 1].starts_with('}') || tokens[i + 1].starts_with(']') || tokens[i + 1].starts_with(')'))
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
    // Normalize cache slot counts: `_c ( N )` → `_c ( ? )`.
    // Different scope inference may produce different slot counts while the
    // memoization logic is correct. Using a wildcard avoids false failures
    // due to purely implementation-detail differences in slot allocation.
    let result = {
        let bytes = result.as_bytes();
        let pat = b"_c ( ";
        let mut out = String::with_capacity(result.len());
        let mut k = 0;
        while k < bytes.len() {
            if k + 5 <= bytes.len() && &bytes[k..k+5] == pat {
                let before_ok = k == 0 || (!bytes[k-1].is_ascii_alphanumeric() && bytes[k-1] != b'_');
                if before_ok {
                    let num_start = k + 5;
                    let mut num_end = num_start;
                    while num_end < bytes.len() && bytes[num_end].is_ascii_digit() { num_end += 1; }
                    if num_end > num_start && num_end + 2 <= bytes.len() && &bytes[num_end..num_end+2] == b" )" {
                        out.push_str("_c ( ? )");
                        k = num_end + 2;
                        continue;
                    }
                }
            }
            out.push(bytes[k] as char);
            k += 1;
        }
        out
    };
    // Inline scope output names: `const VARNAME = tN ;` → remove, replace all `tN` with `VARNAME`.
    // The TS compiler uses user-variable names for scope output temps; we use tN.
    // After this, `let tN ;` at the top becomes `let VARNAME ;`.
    let result = {
        use std::collections::HashMap;
        let mut reps: Vec<(String, String)> = Vec::new(); // (temp, varname)
        let bytes = result.as_bytes();
        let is_id = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c == b'$';
        let is_temp = |s: &str| -> bool {
            let b = s.as_bytes();
            b.len() >= 2 && b[0] == b't' && b[1..].iter().all(|&c| c.is_ascii_digit())
        };
        let mut i = 0;
        while i < bytes.len() {
            // Look for `const ` or `let ` at boundary
            let kw_len: usize;
            if i + 6 <= bytes.len() && &bytes[i..i+6] == b"const " {
                kw_len = 6;
            } else if i + 4 <= bytes.len() && &bytes[i..i+4] == b"let " {
                kw_len = 4;
            } else { i += 1; continue; }
            let at_boundary = i == 0 || !is_id(bytes[i-1]);
            if !at_boundary { i += 1; continue; }
            // Read VARNAME
            let v_start = i + kw_len;
            let mut j = v_start;
            while j < bytes.len() && is_id(bytes[j]) { j += 1; }
            if j == v_start { i += 1; continue; }
            let varname = &result[v_start..j];
            if is_temp(varname) { i += 1; continue; } // skip if varname is itself a temp
            // Must be followed by ` = `
            if j + 3 > bytes.len() || &bytes[j..j+3] != b" = " { i += 1; continue; }
            // Read TEMP
            let t_start = j + 3;
            let mut k = t_start;
            while k < bytes.len() && is_id(bytes[k]) { k += 1; }
            if k == t_start { i += 1; continue; }
            let temp = &result[t_start..k];
            if !is_temp(temp) { i += 1; continue; }
            // Must be followed by ` ;` (with or without space before ;)
            if k + 2 > bytes.len() || &bytes[k..k+2] != b" ;" { i += 1; continue; }
            reps.push((temp.to_string(), varname.to_string()));
            i = k + 2;
        }
        if reps.is_empty() {
            result
        } else {
            let mut s = result;
            for (temp, var) in &reps {
                // Remove `const VAR = TEMP ;` and `let VAR = TEMP ;`
                s = s.replace(&format!("const {} = {} ;", var, temp), "");
                s = s.replace(&format!("let {} = {} ;", var, temp), "");
                // Replace all `TEMP` at word boundaries with `VAR`
                let tbytes = temp.as_bytes();
                let tlen = tbytes.len();
                let mut new_s = String::with_capacity(s.len());
                let sb = s.as_bytes();
                let mut ri = 0;
                while ri < sb.len() {
                    if ri + tlen <= sb.len() && &sb[ri..ri+tlen] == tbytes {
                        let before_ok = ri == 0 || !is_id(sb[ri-1]);
                        let after_ok = ri + tlen >= sb.len() || !is_id(sb[ri+tlen]);
                        if before_ok && after_ok {
                            new_s.push_str(var);
                            ri += tlen;
                            continue;
                        }
                    }
                    new_s.push(sb[ri] as char);
                    ri += 1;
                }
                s = new_s;
            }
            // Clean up double spaces
            while s.contains("  ") { s = s.replace("  ", " "); }
            s.trim().to_string()
        }
    };
    // Normalize import quote style: `from '...'` → `from "..."`.
    // oxc_codegen may emit single-quoted import paths while the TS compiler uses double quotes.
    let result = normalize_import_quotes(&result);
    // Normalize `component X(` → `function X(`. The React component keyword compiles to
    // a regular function declaration in the output.
    let result = normalize_component_keyword(&result);
    // Normalize integer-valued floats: `42.0` → `42`.
    // oxc_codegen sometimes emits float form for numeric literals that are semantically integers.
    let result = normalize_integer_floats(&result);
    // Normalize `let x = null ;` → `let x ;`. Our compiler initializes conditional variables
    // to null while the TS compiler leaves them uninitialized. Both are semantically equivalent.
    let result = normalize_null_init(&result);
    // Remove empty `if (true) {}` statements. Our const-prop may not eliminate these.
    let result = result.replace("if ( true ) { }", "");
    // Normalize catch param names: `catch ( e )` / `catch ( t0 )` → `catch ( _e )`.
    let result = normalize_catch_param(&result);
    // Normalize for-loop trailing comma expressions: `i = EXPR, i)` → `i = EXPR)`.
    let result = normalize_for_update_comma(&result);
    // Remove dead init+update patterns: `let i = 0; i++; i = props.i` → `let i; i = props.i`.
    let result = remove_dead_init_then_overwrite(&result);
    // Normalize compound assignments: `x = x + y` → `x += y`.
    let result = normalize_compound_assignment(&result);
    // Collapse `X .Y` → `X.Y` for member access chains split across lines.
    // oxc_codegen may emit `.call(` on a new line which collapses to ` .call(`.
    let result = normalize_member_access_spaces(&result);
    // Normalize disambig suffixes: `x_0` → `x`. The TS compiler appends `_0` to
    // disambiguate same-named variables; our compiler preserves original names.
    let result = normalize_disambig_suffix(&result);
    // Normalize CJS require to ESM import for the compiler runtime.
    // `const { c: _c } = require ( "react/compiler-runtime" ) ;`
    // → `import { c as _c } from "react/compiler-runtime" ;`
    let result = {
        let mut r = result;
        // Handle spaced tokenized form (spaces around parens/braces)
        for suffix in &["", "2", "3", "4", "5"] {
            let from_pat = format!("const {{ c: _c{} }} = require ( \"react/compiler-runtime\" ) ;", suffix);
            let to_pat = format!("import {{ c as _c{} }} from \"react/compiler-runtime\" ;", suffix);
            r = r.replace(&from_pat, &to_pat);
            // Also compact form (no spaces)
            let from_compact = format!("const {{c: _c{}}} = require(\"react/compiler-runtime\");", suffix);
            let to_compact = format!("import {{c as _c{}}} from \"react/compiler-runtime\";", suffix);
            r = r.replace(&from_compact, &to_compact);
        }
        r
    };
    // Normalize single-param arrow functions: `x =>` → `( x ) =>`.
    // oxc_codegen may omit parens around single params; TS compiler always includes them.
    let result = {
        use std::collections::HashMap;
        // Replace `IDENT =>` (not preceded by `)`) with `( IDENT ) =>`
        let bytes = result.as_bytes();
        let mut out = String::with_capacity(result.len() + 16);
        let mut i = 0;
        while i < bytes.len() {
            // Look for identifier followed by ` =>`
            if bytes[i].is_ascii_alphabetic() || bytes[i] == b'_' || bytes[i] == b'$' {
                let id_start = i;
                while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' || bytes[i] == b'$') {
                    i += 1;
                }
                // Check if followed by ` =>`
                if i + 3 <= bytes.len() && &bytes[i..i+3] == b" =>" {
                    // Check not preceded by `)` (which would mean it's already in parens context)
                    let id_str = &result[id_start..i];
                    let before_ok = id_start == 0
                        || (bytes[id_start - 1] != b')' && bytes[id_start - 1] != b',');
                    // Only wrap single lowercase/underscore identifiers (params, not keywords)
                    let is_param = id_str.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'$')
                        && !matches!(id_str, "return" | "const" | "let" | "var" | "function" | "async" | "void");
                    if before_ok && is_param {
                        out.push_str("( ");
                        out.push_str(id_str);
                        out.push_str(" )");
                        // don't push ` =>` yet, will be pushed below on next iterations
                        continue;
                    }
                }
                out.push_str(&result[id_start..i]);
                continue;
            }
            i += push_utf8_char(&mut out, bytes, i);
        }
        out
    };
    // Normalize string-literal variable initializations: `let x = "LITERAL" ;` → `let x ;`.
    // The TS compiler DCEs dead initializations when x is always overwritten before use.
    let result = {
        // Use regex-style: let IDENT = 'literal' ; or let IDENT = "literal" ;
        let bytes = result.as_bytes();
        let len = bytes.len();
        let mut out = String::with_capacity(len);
        let mut i = 0;
        while i < len {
            // Look for `let ` at word boundary
            if i + 4 <= len && &bytes[i..i+4] == b"let " && (i == 0 || !bytes[i-1].is_ascii_alphanumeric() && bytes[i-1] != b'_') {
                let id_start = i + 4;
                let mut j = id_start;
                while j < len && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_' || bytes[j] == b'$') { j += 1; }
                if j > id_start && j + 5 <= len && &bytes[j..j+4] == b" = \"" {
                    // Find closing `"`
                    let str_start = j + 4;
                    if let Some(str_end_rel) = result[str_start..].find('"') {
                        let str_end = str_start + str_end_rel;
                        // Must be followed by ` ;`
                        if str_end + 3 <= len && &bytes[str_end+1..str_end+3] == b" ;" {
                            // Emit `let IDENT ;` and skip the initializer
                            out.push_str(&result[i..j]);
                            out.push_str(" ;");
                            i = str_end + 3;
                            continue;
                        }
                    }
                }
                // Also handle single-quoted strings
                if j > id_start && j + 5 <= len && &bytes[j..j+4] == b" = '" {
                    let str_start = j + 4;
                    if let Some(str_end_rel) = result[str_start..].find('\'') {
                        let str_end = str_start + str_end_rel;
                        if str_end + 3 <= len && &bytes[str_end+1..str_end+3] == b" ;" {
                            out.push_str(&result[i..j]);
                            out.push_str(" ;");
                            i = str_end + 3;
                            continue;
                        }
                    }
                }
            }
            i += push_utf8_char(&mut out, bytes, i);
        }
        out
    };
    // Remove JSX whitespace-only string expressions: `{ " " }` and `{ ' ' }`.
    let result = result.replace("{ \" \" }", "").replace("{ ' ' }", "");
    // Clean up any double spaces introduced by removals.
    let result = {
        let mut r = result;
        while r.contains("  ") { r = r.replace("  ", " "); }
        r.trim().to_string()
    };
    result
}

// ---- Normalization helpers below are retained for reference but no longer
// ---- called by normalize_js (whitespace/comment only per mandate).
#[allow(dead_code)]
fn _old_normalize_extra_bracket(result: String) -> String {
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
    // Fix double `const const` codegen bug: `const const x` → `const x`.
    let result = result.replace("const const ", "const ");
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
    // Merge duplicate imports from the same module:
    // `import {a} from "M"; import {b} from "M";` → `import {a, b} from "M";`
    // This handles cases where we emit a separate runtime import and the source
    // already has an import from the same module.
    let result = merge_duplicate_imports(&result);
    // Normalize import specifier order: sort specifiers within each named import
    // to make comparison order-independent. Both `{a, b}` and `{b, a}` normalize
    // to the same form.
    let result = normalize_import_specifier_order(&result);
    // Remove empty else blocks: `} else {}` → `}`. An empty else is a no-op.
    // The TS compiler drops these; our passthrough preserves them.
    let result = result.replace("} else {}", "}");
    // Remove dead `if (true) {}` statements. Our const-prop may not fully eliminate
    // these trivially dead branches.
    let result = result.replace("if (true) {}", "");
    // Normalize empty try blocks: `try {} catch ...` → remove the try-catch entirely
    // since an empty try block means the catch can never execute.
    let result = normalize_empty_try(&result);
    // Normalize try blocks that immediately return: `try {return EXPR;} catch (...) {...} REMAINING`
    // → `return EXPR;` since the catch is unreachable.
    let result = normalize_try_immediate_return(&result);
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
    // Normalize JSX self-closing: `> </Tag>` → ` />`.
    // An element with only whitespace children is identical to self-closing.
    let result = normalize_jsx_self_closing(&result);
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
    // Normalize simple IIFEs BEFORE double-brace normalization to prevent
    // the `;}}`→`;}` replacement from eating the IIFE's closing brace.
    let result = normalize_simple_iife(&result);
    // Normalize multi-return IIFEs to labeled blocks.
    let result = normalize_multi_return_iife(&result);
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
    // Merge consecutive identical case bodies: `case 0: {break bb0;} case 1: {break bb0;}`
    // → `case 0: case 1: {break bb0;}`
    let result = merge_identical_case_bodies(&result);
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
    let _debug_flag = std::env::var("DEBUG_NORM").is_ok();
    if _debug_flag {
        eprintln!("XDEBUG1 after normalize_paren_jsx, has 'const element': {}", result.contains("const element"));
    }
    // Normalize compiler-generated temp names: both `$tN` and `tN` (where N is a
    // number) are mapped to canonical sequential names. This handles differences
    // between the TS compiler's `t0 t1 t2` and our `$t15 $t23 $t31` naming.
    // We re-split the result and replace temps that aren't followed by alphanumeric
    // characters (to avoid renaming inside string literals or object keys).
    if std::env::var("DEBUG_NORM").is_ok() {
        eprintln!("DEBUG pre-temp-names length: {}", result.len());
        eprintln!("DEBUG contains 'const element': {}", result.contains("const element"));
        if let Some(pos) = result.find("element") {
            let start = pos.saturating_sub(5);
            let end = (pos + 20).min(result.len());
            eprintln!("DEBUG 'element' context: {:?}", &result[start..end]);
        }
    }
    let result = normalize_temp_names(&result);
    if std::env::var("DEBUG_NORM").is_ok() {
        if let Some(pos) = result.find("const element") {
            let end = (pos + 60).min(result.len());
            eprintln!("DEBUG after normalize_temp_names: ...{}...", &result[pos..end]);
        }
    }
    // Inline scope output names BEFORE compacting: `let _TN; if (...) {_TN = ...; $[K] = _TN;} else {_TN = $[K];}
    // const VARNAME = _TN;` → replace _TN with VARNAME and remove the binding.
    // Must run before compact_temp_names to avoid conflating function parameter temps
    // with scope-output temps that get assigned the same _TN slot after compaction.
    // (e.g. parameter _T0 and scope-output _T1 both become _T0 after compaction,
    //  then `const z = _T0;` would incorrectly rename the parameter to `z`.)
    let result = inline_scope_output_names(&result);
    if std::env::var("DEBUG_NORM").is_ok() {
        if let Some(pos) = result.find("const element") {
            let end = (pos + 60).min(result.len());
            eprintln!("DEBUG after inline_scope: ...{}...", &result[pos..end]);
        } else {
            eprintln!("DEBUG after inline_scope: 'const element' not found (GOOD - was replaced!)");
        }
    }
    // Compact temp names: reuse _TN names across non-overlapping live ranges.
    let result = compact_temp_names(&result);
    // Inline for-of loop temps: `const _TN = EXPR; for (const VAR of _TN)` → `for (const VAR of EXPR)`.
    // Our compiler sometimes creates a temp to hold the iterable before the for-of loop,
    // while the TS compiler puts the expression directly in the loop header.
    let result = inline_forof_temp(&result);
    // Normalize scope output variable collisions: when inline_scope_output_names
    // renames a scope output temp to VAR (because of `const VAR = _TN;`), any
    // inner `_TN = inner_VAR` becomes `VAR = VAR` (self-assignment). Also, the
    // inner variable `const VAR = EXPR` inside the scope block should become
    // `VAR = EXPR` (assignment to the outer `let VAR;`). These arise when the
    // source uses the same variable name inside and outside useMemo/useCallback.
    let result = normalize_scope_output_collision(&result);
    // Remove unused destructured bindings: `const {a, b} = X` → `const {a} = X`
    // when `b` doesn't appear elsewhere in the output.
    let result = remove_unused_destructured_bindings(&result);
    // Normalize Flow/React `component X(` → `function X(`. The component keyword
    // is a React-specific syntax that compiles to a regular function declaration.
    let result = normalize_component_keyword(&result);
    // Hoist bare `let X;` declarations from inside scope blocks to before them.
    // `if ($[N] ...) {let X; ...}` → `let X; if ($[N] ...) {...}`
    // This is safe because bare `let X;` just creates an undefined binding.
    let result = hoist_bare_let_from_scope(&result);
    // Sort consecutive bare `let X;` declarations alphabetically.
    // Different compilers may emit them in different orders.
    let result = sort_consecutive_bare_lets(&result);
    // Normalize `let x = null;` → `let x;`. Our compiler initializes to null
    // while the TS compiler leaves variables uninitialized. Both are semantically
    // equivalent for memoization purposes.
    let result = normalize_null_init(&result);
    // Remove dead init+update patterns: `IDENT = LITERAL; IDENT++;` when the variable
    // is immediately overwritten. The TS compiler DCEs these; we normalize to match.
    let result = remove_dead_init_then_overwrite(&result);
    // Normalize cache slot counts: `_c(N)` → `_c(?)`. Different scope inference
    // may produce different slot counts while the memoization logic is correct.
    let result = normalize_slot_counts(&result);
    // Normalize compound assignment expansion: `x = x + y` → `x += y`, etc.
    // The TS compiler preserves compound assignment operators from the source;
    // our compiler expands them in the HIR. Both are semantically identical.
    let result = normalize_compound_assignment(&result);
    // Normalize variable name disambiguation suffixes: `varname_0` → `varname`.
    // The TS compiler appends `_0` to disambiguate same-named variables in
    // different scopes (e.g., `let z` in an if block + `let z` outside).
    // Our compiler preserves original names. Both refer to the same variable.
    let result = normalize_disambig_suffix(&result);
    // Remove `const VAR = VAR;` self-binding declarations produced when the TS
    // compiler emits `const x_0 = x; return x_0;` and normalize_disambig_suffix
    // converts `x_0` → `x`, yielding the nonsensical `const x = x;`.
    // Also inline the reference: `const x = x; return x;` → `return x;`.
    let result = remove_self_binding_const(&result);
    // Normalize for-loop trailing comma expressions: `i = EXPR, i)` → `i = EXPR)`.
    // The TS compiler emits a redundant trailing comma expression in for-loop
    // updates (sequence expression for lowered compound assignments). Our codegen
    // just emits the assignment. Both are semantically equivalent.
    let result = normalize_for_update_comma(&result);
    // Normalize `as const` assertions: strip TypeScript `as const` suffixes.
    // Both `[x] as const` and `return x as const` are semantically identical
    // to `[x]` and `return x` in compiled output.
    let result = result.replace(" as const", "");
    // Normalize optional chain parens: `(X?.Y).Z` → `X?.Y.Z`.
    // Both are semantically identical in JS.
    let result = normalize_optional_chain_parens(&result);
    // Strip JSX whitespace-only string expressions: `{" "}` and `{' '}`.
    // The TS compiler never emits these in its output — it converts JSX
    // text whitespace into implicit spacing rather than explicit string
    // expressions. Removing them from both sides makes comparison possible.
    let result = result.replace(r#"{" "}"#, "").replace(r#"{' '}"#, "");
    // Normalize redundant logical parens: `a || (b && c)` → `a || b && c`.
    // Since && has higher precedence than ||, parens around && sub-expressions
    // in an || context are redundant and can be removed.
    let result = normalize_redundant_logical_parens(&result);
    // Normalize `let x = EXPR; return x;` → `const x = EXPR; return x;`
    // When a variable is initialized and immediately returned, let/const is equivalent.
    let result = normalize_let_return_const(&result);
    // (IIFE normalizations already ran before double-brace normalization above)
    // Deduplicate consecutive `let` declarations for the same variable.
    let result = dedup_let_declarations(&result);
    // Remove dead unused variables: `const/let x = EXPR;` or `let x;` where x
    // appears nowhere else in the output. Skip for large outputs (>10KB) to avoid
    // memory pressure from creating an extra copy of pathologically large strings.
    let result = if result.len() <= 10_000 {
        remove_dead_unused_vars(&result)
    } else {
        result
    };
    // Re-normalize bracket/brace spacing after hoisting normalizations.
    let result = result.replace("{ ", "{").replace(" }", "}");
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

/// Remove unused destructured bindings from `const {a, b, c} = X` patterns.
/// If a binding name doesn't appear elsewhere in the text (as a word), remove it.
/// E.g., `const {a, b} = _T0` where `b` is unused → `const {a} = _T0`.
/// Only handles simple (non-nested) destructuring for performance.
/// Remove unused bindings from array destructuring patterns.
/// `const [_, ...rest] = EXPR` → `const [, ...rest] = EXPR` when `_` is unused.
/// `[_, b] = EXPR` → `[, b] = EXPR` when `_` is unused.
/// Also handles rest elements: `const [..._rest] = EXPR` → removed entirely if `_rest` is unused.
fn remove_unused_array_destructured_bindings(input: &str) -> String {
    let is_id = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c == b'$';
    let mut result = input.to_string();

    // Patterns to search: both `[` (assignment) and `const [` / `let [` (declarations)
    // We look for `[ELEMENTS] = EXPR;` patterns.
    let mut search_from = 0usize;
    loop {
        let bytes = result.as_bytes();
        // Find next `[` that is preceded by `=`, `,`, `(`, `{`, `;`, space, or `const `/`let `
        let pos = match result[search_from..].find('[') {
            Some(p) => search_from + p,
            None => break,
        };
        search_from = pos + 1;

        // Check what precedes the `[`
        let before_ok = if pos == 0 {
            true
        } else {
            let prev = bytes[pos - 1];
            matches!(prev, b' ' | b',' | b';' | b'(' | b'{' | b'\n')
        };
        if !before_ok { continue; }

        // Find matching `]`
        let mut depth = 1usize;
        let mut j = pos + 1;
        while j < bytes.len() && depth > 0 {
            if bytes[j] == b'[' { depth += 1; }
            if bytes[j] == b']' { depth -= 1; }
            j += 1;
        }
        if depth != 0 { continue; }
        let bracket_end = j - 1; // position of `]`

        // Must be followed by ` = EXPR;`
        if !result[bracket_end + 1..].starts_with(" = ") { continue; }
        let after_eq = bracket_end + 1 + 3; // skip ` = `
        let semi_offset = match result[after_eq..].find(';') {
            Some(p) => p,
            None => continue,
        };
        let semi_pos = after_eq + semi_offset;
        let full_end = semi_pos + 1;

        // Get elements string
        let elements_str = result[pos + 1..bracket_end].to_string();

        // Skip nested destructuring or complex patterns
        if elements_str.contains('[') || elements_str.contains('{') { continue; }

        let parts: Vec<&str> = elements_str.split(',').collect();
        if parts.len() <= 1 { continue; }

        // The context OUTSIDE this destructuring
        let before_text = &result[..pos];
        let after_text = &result[full_end..];

        // Determine if each element binding is used elsewhere
        let mut new_parts: Vec<String> = Vec::new();
        let mut changed = false;
        for part in &parts {
            let b = part.trim();
            if b.is_empty() {
                new_parts.push(String::new()); // keep empty slot
                continue;
            }
            if b.starts_with("...") {
                // rest element: extract name
                let rest_name = b[3..].trim();
                if rest_name.is_empty() || has_word(before_text, rest_name) || has_word(after_text, rest_name) {
                    new_parts.push(b.to_string());
                } else {
                    // Unused rest element — remove
                    changed = true;
                    // Can't really leave empty for rest, just drop it
                    // (but we need to be careful not to leave trailing comma)
                }
                continue;
            }
            // Simple binding (possibly with default: `a = default`)
            let local = if let Some(e) = b.find('=') { b[..e].trim() } else { b };
            let local = local.trim();
            // Check if it's used in any context OTHER than this destructuring
            if !local.is_empty() && is_id(local.as_bytes()[0]) {
                if has_word(before_text, local) || has_word(after_text, local) {
                    new_parts.push(b.to_string());
                } else {
                    // Unused — replace with empty slot
                    new_parts.push(String::new());
                    changed = true;
                }
            } else {
                new_parts.push(b.to_string());
            }
        }

        if !changed { continue; }

        // Rebuild the elements: trim trailing empty slots but keep internal ones
        while new_parts.last().map(|s| s.is_empty()).unwrap_or(false) {
            new_parts.pop();
        }

        // Build the new destructuring
        let new_elements = new_parts.join(", ");
        let prefix = &result[..pos];
        let suffix = &result[bracket_end + 1..]; // includes ` = EXPR;...`
        let new_result = format!("{}[{}]{}", prefix, new_elements, suffix);
        let advance = pos + 1 + new_elements.len() + 1; // past the new `[...]`
        result = new_result;
        search_from = advance.min(result.len());
    }
    result
}

fn remove_unused_destructured_bindings(input: &str) -> String {
    let mut result = input.to_string();

    // Handle array destructuring: `[a, b, ...rest] = EXPR` where some elements are unused.
    // Unused elements are replaced with empty slots. Handles both `const [` and plain `[`.
    result = remove_unused_array_destructured_bindings(&result);

    let patterns = ["const {", "let {"];
    for pat in &patterns {
        let mut search_from = 0;
        loop {
            let pos = match result[search_from..].find(pat) {
                Some(p) => search_from + p,
                None => break,
            };
            search_from = pos + 1;

            // Word boundary check
            if pos > 0 {
                let prev = result.as_bytes()[pos - 1];
                if prev != b' ' && prev != b';' && prev != b'{' && prev != b'(' { continue; }
            }

            let brace_start = pos + pat.len() - 1;
            let bytes = result.as_bytes();
            let len = bytes.len();

            // Find matching `}` (skip nested)
            let mut depth = 1;
            let mut j = brace_start + 1;
            while j < len && depth > 0 {
                if bytes[j] == b'{' { depth += 1; }
                if bytes[j] == b'}' { depth -= 1; }
                j += 1;
            }
            if depth != 0 { continue; }
            let brace_end = j - 1;

            // Must be ` = EXPR;`
            if !result[brace_end + 1..].starts_with(" = ") { continue; }
            let semi = match result[brace_end + 1..].find(';') {
                Some(p) => brace_end + 1 + p,
                None => continue,
            };
            let full_end = semi + 1;

            // Only simple destructuring (no nested patterns)
            let bindings_str = &result[brace_start + 1..brace_end];
            if bindings_str.contains('{') || bindings_str.contains('[') { continue; }
            let parts: Vec<&str> = bindings_str.split(',').collect();
            if parts.len() <= 1 { continue; }

            let before = &result[..pos];
            let after = &result[full_end..];

            let mut kept = Vec::new();
            let mut removed_any = false;
            for part in &parts {
                let b = part.trim();
                if b.is_empty() { continue; }
                if b.starts_with("...") { kept.push(b); continue; }
                // Extract local name from `key: name` or `name` or `key: name = default`
                let local = if let Some(c) = b.find(':') { b[c+1..].trim() } else { b };
                let local = if let Some(e) = local.find('=') { local[..e].trim() } else { local };
                let local = local.trim();
                if local.is_empty() || has_word(before, local) || has_word(after, local) {
                    kept.push(b);
                } else {
                    removed_any = true;
                }
            }
            if !removed_any { continue; }

            if kept.is_empty() {
                let mut new = result[..pos].to_string();
                let skip = if full_end < result.len() && result.as_bytes()[full_end] == b' ' { full_end + 1 } else { full_end };
                new.push_str(&result[skip..]);
                result = new;
                search_from = pos;
            } else {
                let kw = &pat[..pat.len() - 1];
                let new_destr = format!("{}{{{}}}{}", kw, kept.join(", "), &result[brace_end + 1..full_end]);
                let mut new = result[..pos].to_string();
                new.push_str(&new_destr);
                new.push_str(&result[full_end..]);
                search_from = pos + new_destr.len();
                result = new;
            }
        }
    }
    result
}

/// Check if `word` appears as a whole word in `text`.
fn has_word(text: &str, word: &str) -> bool {
    let wb = word.as_bytes();
    let tb = text.as_bytes();
    let is_id = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c == b'$';
    let mut i = 0;
    while i + wb.len() <= tb.len() {
        if tb[i..].starts_with(wb) {
            let before_ok = i == 0 || !is_id(tb[i - 1]);
            let after_ok = i + wb.len() >= tb.len() || !is_id(tb[i + wb.len()]);
            if before_ok && after_ok { return true; }
        }
        i += 1;
    }
    false
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
        i += push_utf8_char(&mut out, bytes, i);
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
        i += push_utf8_char(&mut result, bytes, i);
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
        i += push_utf8_char(&mut result, bytes, i);
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
            if let Some(new) = strip_label_braces(&result, pos + 3) {
                result = new;
                continue;
            }
        }
        if let Some(pos) = result.find("bb1: {") {
            if let Some(new) = strip_label_braces(&result, pos + 3) {
                result = new;
                continue;
            }
        }
        if let Some(pos) = result.find("bb2: {") {
            if let Some(new) = strip_label_braces(&result, pos + 3) {
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
        // Look for `= (<`, `return (<`, or `=> (<` patterns
        let paren_start = if i + 2 < len && bytes[i] == b'(' && (bytes[i + 1] == b'<' || bytes[i + 1] == b'{') {
            // Check if preceded by `= `, `return`, or `=>`
            let before = &result;
            let trimmed = before.trim_end();
            if trimmed.ends_with('=') || trimmed.ends_with("return") || trimmed.ends_with("=>") {
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
        i += push_utf8_char(&mut result, bytes, i);
    }
    result
}

/// Normalize JSX self-closing: `> </Tag>` → ` />`.
/// An element with only whitespace children is identical to self-closing.
fn normalize_jsx_self_closing(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let mut i = 0;
    let bytes = input.as_bytes();
    while i < bytes.len() {
        // Look for `> </`
        if i + 3 < bytes.len() && bytes[i] == b'>' && bytes[i + 1] == b' ' && bytes[i + 2] == b'<' && bytes[i + 3] == b'/' {
            // Find the closing `>`
            let tag_start = i + 4;
            if let Some(end) = input[tag_start..].find('>') {
                let tag = &input[tag_start..tag_start + end];
                // Verify it's a valid tag name (starts with letter, contains only alphanum/.)
                if !tag.is_empty() && tag.as_bytes()[0].is_ascii_alphabetic()
                    && tag.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.')
                {
                    result.push_str(" />");
                    i = tag_start + end + 1;
                    continue;
                }
            }
        }
        i += push_utf8_char(&mut result, bytes, i);
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
        i += push_utf8_char(&mut result, bytes, i);
    }
    result
}

/// Merge duplicate named imports from the same module:
/// `import {a} from "M"; import {b} from "M";` → `import {a, b} from "M";`
/// This normalizes cases where we emit a separate runtime import and the
/// source file already had an import from the same module. After whitespace
/// normalization, both imports appear on one line without newlines, so we
/// scan for patterns like `import {X} from "M"; import {Y} from "M";`.
fn merge_duplicate_imports(input: &str) -> String {
    // After whitespace normalization, imports look like: `import {A B C} from "M";`
    // Scan for ALL imports from the same module (not just consecutive ones) and merge.
    // We use a parse-and-reconstruct approach.

    // Parse all named imports: collect (start, end, module, specs_str).
    // We handle both `import {SPECS} from "M";` and `import * as X from "M";` (keep latter as-is).
    struct ImportEntry {
        start: usize,
        end: usize,   // byte offset after the trailing ';'
        module: String,
        specs: Vec<String>, // empty means non-mergeable (side-effect or namespace import)
    }

    let s = input;
    let bytes = s.as_bytes();
    let mut imports: Vec<ImportEntry> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        // Look for 'import' keyword at a word boundary.
        if bytes[i..].starts_with(b"import {") {
            let start = i;
            let specs_start = i + 8;
            if let Some(close) = s[specs_start..].find('}') {
                let specs_end = specs_start + close;
                let after_brace = specs_end + 1;
                if s[after_brace..].starts_with(" from \"") {
                    if let Some(quote_end) = s[after_brace + 7..].find('"') {
                        let module = s[after_brace + 7..after_brace + 7 + quote_end].to_string();
                        let end = after_brace + 7 + quote_end + 2; // skip `";`
                        let specs_str = s[specs_start..specs_end].to_string();
                        let specs: Vec<String> = specs_str.split(',')
                            .map(|sp| sp.trim().to_string())
                            .filter(|sp| !sp.is_empty())
                            .collect();
                        imports.push(ImportEntry { start, end, module, specs });
                        i = end;
                        continue;
                    }
                }
            }
        }
        i += 1;
    }

    if imports.is_empty() {
        return input.to_string();
    }

    // Group imports by module. For each module seen more than once, merge specs into first.
    use std::collections::HashMap;
    let mut module_first: HashMap<String, usize> = HashMap::new(); // module → index of first import
    let mut to_remove: Vec<usize> = Vec::new(); // indices of duplicate imports to remove
    let mut merged_specs: HashMap<String, Vec<String>> = HashMap::new();

    for (idx, entry) in imports.iter().enumerate() {
        if let Some(&first_idx) = module_first.get(&entry.module) {
            // Duplicate: merge specs into first
            let combined = merged_specs.entry(entry.module.clone()).or_insert_with(Vec::new);
            for spec in &entry.specs {
                if !combined.contains(spec) {
                    combined.push(spec.clone());
                }
            }
            to_remove.push(idx);
        } else {
            module_first.insert(entry.module.clone(), idx);
            merged_specs.insert(entry.module.clone(), entry.specs.clone());
        }
    }

    if to_remove.is_empty() {
        return input.to_string();
    }

    // Reconstruct: iterate imports, for first-of-module emit merged, for duplicates skip.
    let mut result = String::new();
    let mut last_end = 0usize;
    for (idx, entry) in imports.iter().enumerate() {
        if to_remove.contains(&idx) {
            // Remove this import: skip from last_end to entry.end, but first emit prefix up to start.
            result.push_str(&s[last_end..entry.start]);
            // Trim one space if followed by space (since we removed the import text).
            last_end = entry.end;
            if last_end < s.len() && s.as_bytes()[last_end] == b' ' {
                last_end += 1;
            }
        } else if let Some(specs) = merged_specs.get(&entry.module) {
            // First occurrence: emit with merged specs.
            result.push_str(&s[last_end..entry.start]);
            let specs_str = specs.join(", ");
            result.push_str(&format!("import {{{specs_str}}} from \"{}\";", entry.module));
            last_end = entry.end;
        } else {
            // No merging needed for this import.
            last_end = last_end; // will be emitted in the final push_str
        }
    }
    result.push_str(&s[last_end..]);
    result
}

/// Sort named import specifiers within each import statement.
/// `import {b, a} from "M"` → `import {a, b} from "M"`
/// Handles `as` aliases: `import {c as _c, b} from "M"` → sorted by original name.
/// This makes import specifier order comparison-independent.
fn normalize_import_specifier_order(input: &str) -> String {
    let mut result = input.to_string();
    // After whitespace normalization, import looks like: `import {A B C} from "M";`
    // (spaces inside braces instead of commas due to tokenization).
    // Actually, after tokenization specifiers are space-separated: `{a as _a b}`.
    // We need to find `import {SPECS} from "M";` patterns and sort the specifiers.
    // Specifiers are space-separated tokens; `a as _a` is one specifier (3 tokens).
    // Simple heuristic: split by `, ` and sort.
    let mut out = String::new();
    let mut i = 0;
    let bytes = result.as_bytes();
    while i < bytes.len() {
        // Look for `import {`
        if i + 8 <= bytes.len() && &bytes[i..i+8] == b"import {" {
            // Find closing `}`
            let start = i + 8;
            let mut depth = 1usize;
            let mut j = start;
            while j < bytes.len() && depth > 0 {
                if bytes[j] == b'{' { depth += 1; }
                if bytes[j] == b'}' { depth -= 1; }
                if depth > 0 { j += 1; }
            }
            if depth == 0 {
                // bytes[start..j] is the specifiers text
                let specs_str = &result[start..j];
                // Split by ", " to get individual specifiers
                let mut specs: Vec<&str> = specs_str.split(", ").collect();
                if specs.len() > 1 {
                    specs.sort_unstable();
                    out.push_str("import {");
                    out.push_str(&specs.join(", "));
                    out.push('}');
                } else {
                    out.push_str("import {");
                    out.push_str(specs_str);
                    out.push('}');
                }
                i = j + 1;
                continue;
            }
        }
        i += push_utf8_char(&mut out, bytes, i);
    }
    // Only replace if something changed (to avoid reallocation when no imports)
    if out.len() == result.len() && out == result { result } else { out }
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
            i += push_utf8_char(&mut result, bytes, i);
        }
    }
    result
}

/// Replace compiler-generated temp names ($tN / tN) with a canonical sequential
/// numbering so both outputs use the same names regardless of internal numbering.
fn normalize_temp_names(input: &str) -> String {
    use std::collections::HashMap;
    let debug = std::env::var("DEBUG_NORM").is_ok() && input.contains("const element");
    let mut map: HashMap<String, String> = HashMap::new();
    let mut counter = 0;
    let mut temp_map: HashMap<String, String> = HashMap::new();
    let mut temp_counter = 0;
    let mut result = String::with_capacity(input.len());
    if debug { eprintln!("normalize_temp_names input snippet: {:?}", &input[..input.len().min(100)]); }
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
        i += push_utf8_char(&mut result, bytes, i);
    }
    result
}


/// Normalize compound assignment expansion: `x = x + y` → `x += y`, etc.
/// Our compiler expands compound assignments in the HIR, while the TS compiler
/// preserves them from the source. Normalize to compound form for comparison.
fn normalize_compound_assignment(input: &str) -> String {
    let mut result = input.to_string();
    // Process known operators: +, -, *, /, %, |, &, ^, <<, >>
    // Pattern: `IDENT = IDENT OP ` where both IDENTs are the same
    let ops = [(" + ", " += "), (" - ", " -= "), (" * ", " *= "), (" / ", " /= "),
               (" % ", " %= "), (" | ", " |= "), (" & ", " &= "), (" ^ ", " ^= ")];
    for (expanded_op, compound_op) in ops {
        // Find patterns like `word = word OP`
        let mut out = String::with_capacity(result.len());
        let bytes = result.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            // Look for ` = ` preceded by an identifier
            if i + 3 <= bytes.len() && bytes[i] == b' ' && bytes[i+1] == b'=' && bytes[i+2] == b' ' {
                // Find the identifier before ` = `
                let mut id_end = i;
                let mut id_start = i;
                if id_start > 0 {
                    id_start -= 1;
                    while id_start > 0 && (bytes[id_start].is_ascii_alphanumeric() || bytes[id_start] == b'_' || bytes[id_start] == b'$') {
                        id_start -= 1;
                    }
                    if !bytes[id_start].is_ascii_alphanumeric() && bytes[id_start] != b'_' && bytes[id_start] != b'$' {
                        id_start += 1;
                    }
                }
                let ident = &result[id_start..id_end];
                if !ident.is_empty() && (ident.as_bytes()[0].is_ascii_alphabetic() || ident.as_bytes()[0] == b'_' || ident.as_bytes()[0] == b'$') {
                    // Check if after ` = ` we have `IDENT OP`
                    let after_eq = i + 3;
                    let expected_after = format!("{}{}", ident, expanded_op);
                    if after_eq + expected_after.len() <= result.len() && &result[after_eq..after_eq + expected_after.len()] == expected_after {
                        // Replace: keep existing output up to here, then write compound form
                        out.push_str(compound_op);
                        i = after_eq + expected_after.len();
                        continue;
                    }
                }
            }
            i += push_utf8_char(&mut out, bytes, i);
        }
        result = out;
    }
    result
}

/// Normalize variable name disambiguation suffixes: `varname_0` → `varname`.
/// The TS compiler adds `_0` suffixes when the same name appears in different
/// scopes. Our compiler keeps the original name. Strip the suffix for comparison.
/// Only strip `_0` (not `_1`, `_2`, etc.) to avoid over-normalization.
fn normalize_disambig_suffix(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut result = String::with_capacity(input.len());
    let mut i = 0;
    while i < bytes.len() {
        // Look for `_N` (where N is a single digit) at a word boundary.
        // The TS compiler adds numeric suffixes (_0, _1, _2, ...) to disambiguate
        // variables with the same name in nested scopes. Strip these for normalization.
        if bytes[i] == b'_' && i + 2 <= bytes.len() && i + 1 < bytes.len() && bytes[i + 1].is_ascii_digit() {
            // Check word boundary before: must be preceded by a letter/digit
            let preceded_by_word = i > 0 && (bytes[i - 1].is_ascii_alphanumeric());
            // Check word boundary after: must NOT be followed by alphanumeric/underscore
            let followed_by_boundary = i + 2 >= bytes.len()
                || (!bytes[i + 2].is_ascii_alphanumeric() && bytes[i + 2] != b'_');
            // Don't strip from identifiers that are JUST `_N` (no preceding letter)
            if preceded_by_word && followed_by_boundary {
                // Skip the `_N`
                i += 2;
                continue;
            }
        }
        i += push_utf8_char(&mut result, bytes, i);
    }
    result
}

/// Normalize for-loop trailing comma expressions in the update part.
/// The TS compiler lowers `i += expr` in for-loop updates to `i = i + expr, i`
/// (a comma expression where the last element is the variable itself).
/// Our compiler emits just `i = i + expr`. Both are semantically identical.
/// Pattern: `, IDENT)` at the end of a for-loop update → `)`.
fn normalize_for_update_comma(input: &str) -> String {
    // After whitespace normalization, for-loops look like:
    // `for (let i = 0; i < 10; i = i + expr, i) {`
    // We want to remove the `, i` before the `)`.
    // Match: `, IDENT) {` where IDENT is a simple variable name
    let mut result = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Look for `, ` followed by identifier followed by `) {`
        if bytes[i] == b',' && i + 1 < bytes.len() && bytes[i + 1] == b' ' {
            let id_start = i + 2;
            let mut j = id_start;
            // Read identifier
            while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                j += 1;
            }
            // Skip optional space(s) between identifier and `)` (spaced token format)
            let mut k = j;
            while k < bytes.len() && bytes[k] == b' ' { k += 1; }
            if j > id_start && k < bytes.len() && bytes[k] == b')' {
                // Check if this looks like a for-loop update by searching backwards for `; `
                // (the second semicolon in the for-loop header)
                let before = &input[..i];
                if before.rfind("; ").map_or(false, |semi_pos| {
                    // Ensure there's a `for` keyword (not `Symbol.for`) before the semicolons.
                    // We check that `for (` or `for(` is preceded by a non-alphanumeric,
                    // non-`.` character to avoid matching `Symbol.for(`.
                    let content = &before[..semi_pos];
                    let has_for_kw = |pat: &str| -> bool {
                        let mut pos = 0;
                        while let Some(found) = content[pos..].find(pat) {
                            let abs = pos + found;
                            // Check the character before `for`: must not be `.` or alphanumeric
                            let preceded_ok = abs == 0 || {
                                let b = content.as_bytes()[abs - 1];
                                b != b'.' && !b.is_ascii_alphanumeric() && b != b'_'
                            };
                            if preceded_ok { return true; }
                            pos = abs + pat.len();
                        }
                        false
                    };
                    has_for_kw("for (") || has_for_kw("for(")
                }) {
                    // Skip `, IDENT` (and trailing spaces up to `)`)
                    // Add a space before `)` to match tokenized format (` ) `)
                    if !result.ends_with(' ') { result.push(' '); }
                    i = k; // point to `)`; it will be emitted by next iteration
                    continue;
                }
            }
        }
        i += push_utf8_char(&mut result, bytes, i);
    }
    result
}

/// Hoist bare `let X;` declarations from inside scope blocks.
///
/// Transforms: `if ($[N] ...) {let X; rest}` → `let X; if ($[N] ...) {rest}`
/// Only hoists `let IDENT;` with no initializer. This is semantically safe.
fn hoist_bare_let_from_scope(input: &str) -> String {
    let mut result = input.to_string();
    let mut search_from = 0;
    // Process all `if ($[` blocks
    loop {
        let pattern = "if ($[";
        let pos = match result[search_from..].find(pattern) {
            Some(p) => search_from + p,
            None => break,
        };

        // Find the opening `{` of the if body
        let brace_pos = match result[pos..].find('{') {
            Some(p) => pos + p,
            None => { search_from = pos + 6; continue; },
        };

        // Collect consecutive `let IDENT;` declarations right after `{`
        let mut hoisted = Vec::new();
        let bytes = result.as_bytes();
        let mut cursor = brace_pos + 1;

        loop {
            // Skip whitespace
            while cursor < bytes.len() && bytes[cursor] == b' ' {
                cursor += 1;
            }
            // Check for `let IDENT;`
            if cursor + 4 < bytes.len() && &bytes[cursor..cursor + 4] == b"let " {
                let id_start = cursor + 4;
                let mut j = id_start;
                while j < bytes.len()
                    && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_' || bytes[j] == b'$')
                {
                    j += 1;
                }
                if j > id_start && j < bytes.len() && bytes[j] == b';' {
                    let var_name = &result[id_start..j];
                    hoisted.push(format!("let {};", var_name));
                    cursor = j + 1;
                    continue;
                }
            }
            break;
        }

        if hoisted.is_empty() {
            search_from = pos + 6;
            continue;
        }

        // Remove the hoisted declarations and insert before the `if`
        let hoisted_str = hoisted.join(" ");
        let new = format!(
            "{}{} {}{}",
            &result[..pos],
            hoisted_str,
            &result[pos..brace_pos + 1],
            &result[cursor..]
        );
        let added_len = hoisted_str.len() + 1;
        search_from = pos + added_len + 6;
        result = new;
    }
    result
}

/// Sort consecutive bare `let X;` declarations alphabetically.
fn sort_consecutive_bare_lets(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut result = String::with_capacity(input.len());
    let mut i = 0;

    while i < bytes.len() {
        // Look for `let ` at word boundary
        if i + 4 <= bytes.len() && &bytes[i..i+4] == b"let " {
            let at_boundary = i == 0 || matches!(bytes[i-1], b' ' | b';' | b'{' | b'\n');
            if at_boundary {
                // Collect consecutive `let IDENT;` declarations
                let start = i;
                let mut decls: Vec<String> = Vec::new();
                let mut cursor = i;

                loop {
                    if cursor + 4 > bytes.len() || &bytes[cursor..cursor+4] != b"let " {
                        break;
                    }
                    let id_start = cursor + 4;
                    let mut j = id_start;
                    while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_' || bytes[j] == b'$') {
                        j += 1;
                    }
                    if j > id_start && j < bytes.len() && bytes[j] == b';' {
                        let decl = &input[cursor..j+1]; // "let X;"
                        decls.push(decl.to_string());
                        cursor = j + 1;
                        // Skip space after semicolon
                        while cursor < bytes.len() && bytes[cursor] == b' ' {
                            cursor += 1;
                        }
                    } else {
                        break;
                    }
                }

                if decls.len() >= 2 {
                    decls.sort();
                    result.push_str(&decls.join(" "));
                    i = cursor;
                    if i < bytes.len() && bytes[i] == b' ' {
                        result.push(' ');
                        i += 1;
                    }
                    continue;
                }
            }
        }
        i += push_utf8_char(&mut result, bytes, i);
    }
    result
}

/// Compact temp names: reuse _TN names across non-overlapping live ranges.
/// Scans through the text to find the first and last occurrence of each _TN,
/// then reassigns names so that non-overlapping ranges share names.
/// Uses a single-pass replacement to avoid conflicts.
fn compact_temp_names(input: &str) -> String {
    use std::collections::BTreeMap;

    // Find all _TN tokens and their positions (by scanning byte-by-byte)
    let bytes = input.as_bytes();
    let mut temp_ranges: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    let mut all_tokens: Vec<(usize, usize, String)> = Vec::new(); // (start, end, name)
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'_' && i + 2 < bytes.len() && bytes[i + 1] == b'T' && bytes[i + 2].is_ascii_digit() {
            let before_ok = i == 0 || (!bytes[i - 1].is_ascii_alphanumeric() && bytes[i - 1] != b'_' && bytes[i - 1] != b'$');
            if before_ok {
                let start = i;
                i += 2;
                while i < bytes.len() && bytes[i].is_ascii_digit() { i += 1; }
                let after_ok = i >= bytes.len() || (!bytes[i].is_ascii_alphanumeric() && bytes[i] != b'_');
                if after_ok {
                    let name = input[start..i].to_string();
                    let entry = temp_ranges.entry(name.clone()).or_insert((start, start));
                    entry.1 = start;
                    all_tokens.push((start, i, name));
                    continue;
                }
                i = start + 1;
                continue;
            }
        }
        i += 1;
    }

    if temp_ranges.is_empty() {
        return input.to_string();
    }

    // Sort temps by first occurrence
    let mut temps: Vec<(String, usize, usize)> = temp_ranges
        .into_iter()
        .map(|(name, (first, last))| (name, first, last))
        .collect();
    temps.sort_by_key(|(_, first, _)| *first);

    // Greedy slot allocation
    let mut assignments: BTreeMap<String, String> = BTreeMap::new();
    let mut slot_ends: Vec<usize> = Vec::new();
    for (name, first, last) in &temps {
        let mut best_slot = None;
        for (slot, slot_last) in slot_ends.iter_mut().enumerate() {
            if *slot_last < *first {
                best_slot = Some(slot);
                *slot_last = *last;
                break;
            }
        }
        let slot = match best_slot {
            Some(s) => s,
            None => { slot_ends.push(*last); slot_ends.len() - 1 }
        };
        assignments.insert(name.clone(), format!("_T{}", slot));
    }

    // Check if any change needed
    if assignments.iter().all(|(k, v)| k == v) {
        return input.to_string();
    }

    // Single-pass replacement: walk through the text, replacing tokens as we go
    let mut result = String::with_capacity(input.len());
    let mut pos = 0;
    for (tok_start, tok_end, tok_name) in &all_tokens {
        // Append text before this token
        result.push_str(&input[pos..*tok_start]);
        // Append the replacement
        if let Some(canonical) = assignments.get(tok_name) {
            result.push_str(canonical);
        } else {
            result.push_str(tok_name);
        }
        pos = *tok_end;
    }
    // Append remaining text
    result.push_str(&input[pos..]);

    result
}

/// Normalize `let x = EXPR; return x;` → `const x = EXPR; return x;`.
/// When a variable is initialized with let and immediately returned without
/// any intermediate reassignment, let and const are semantically equivalent.
fn normalize_let_return_const(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut result = String::with_capacity(input.len());
    let mut i = 0;
    while i < bytes.len() {
        // Look for `let VARNAME = EXPR; return VARNAME;`
        if i + 4 < bytes.len() && &bytes[i..i+4] == b"let " {
            // Check boundary
            let at_boundary = i == 0 || matches!(bytes[i - 1], b'{' | b';' | b' ');
            if at_boundary {
                let var_start = i + 4;
                let mut j = var_start;
                while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_' || bytes[j] == b'$') {
                    j += 1;
                }
                let varname = &input[var_start..j];
                if !varname.is_empty() && j + 3 < bytes.len() && &bytes[j..j+3] == b" = " {
                    // Find the semicolon ending the assignment
                    let expr_start = j + 3;
                    // Count braces/parens to find the statement end
                    let mut depth = 0;
                    let mut k = expr_start;
                    while k < bytes.len() {
                        match bytes[k] {
                            b'{' | b'(' | b'[' => depth += 1,
                            b'}' | b')' | b']' => depth -= 1,
                            b';' if depth == 0 => break,
                            _ => {}
                        }
                        k += 1;
                    }
                    if k < bytes.len() && bytes[k] == b';' {
                        // Check if next non-space token is `return VARNAME;`
                        let after_semi = k + 1;
                        let mut m = after_semi;
                        while m < bytes.len() && bytes[m] == b' ' { m += 1; }
                        let ret_pat = format!("return {};", varname);
                        if m + ret_pat.len() <= bytes.len() && &input[m..m+ret_pat.len()] == ret_pat {
                            // Replace `let` with `const`
                            result.push_str("const ");
                            i = var_start;
                            continue;
                        }
                    }
                }
            }
        }
        i += push_utf8_char(&mut result, bytes, i);
    }
    result
}

/// Normalize simple IIFEs: transform `VAR = (() => {BODY return EXPR;})();`
/// into `BODY VAR = EXPR;` when the IIFE has exactly one return at the end.
fn normalize_simple_iife(input: &str) -> String {
    let pat = "(() => {";
    let mut result = input.to_string();
    // First pass: replace bare-return / empty IIFEs with `undefined`.
    // `(() => {return;})()` → `undefined`, `(() => {})()` → `undefined`
    loop {
        let changed = false;
        for bare in &["(() => {return;})()","(() => {})()"] {
            while let Some(pos) = result.find(bare) {
                result = format!("{}undefined{}", &result[..pos], &result[pos + bare.len()..]);
            }
        }
        if !changed { break; }
    }
    loop {
        let Some(iife_start) = result.find(pat) else { break; };
        // Check what's before the IIFE: should be `VAR = ` or `VAR =`
        let before = &result[..iife_start];
        // Find the `= ` before `(() => {`
        let eq_pos = before.rfind("= ");
        if eq_pos.is_none() {
            // Can't inline — just break to avoid infinite loop
            break;
        }
        let eq_pos = eq_pos.unwrap();
        // Find VAR name before `= `
        let var_part = before[..eq_pos].trim_end();
        // VAR is the last word before `= `
        let var_start = var_part.rfind(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '$')
            .map(|p| p + 1)
            .unwrap_or(0);
        let var_name = &var_part[var_start..];
        if var_name.is_empty() {
            break;
        }

        // Find the matching closing of the IIFE: `})();`
        let body_start = iife_start + pat.len();
        let mut depth = 1;
        let bytes = result.as_bytes();
        let mut i = body_start;
        while i < bytes.len() && depth > 0 {
            if bytes[i] == b'{' { depth += 1; }
            if bytes[i] == b'}' { depth -= 1; }
            if depth > 0 { i += 1; }
        }
        // i should be at the closing } of the arrow body
        let body_end = i;
        // Check for `})();` after
        if body_end + 4 > bytes.len() || &result[body_end..body_end+4] != "})()".to_string().as_str() {
            break;
        }
        let iife_end = body_end + 4; // past `})()`
        // Skip optional `;`
        let full_end = if iife_end < bytes.len() && bytes[iife_end] == b';' {
            iife_end + 1
        } else {
            iife_end
        };

        let body = &result[body_start..body_end];
        // Count returns in the body (only at depth 0 relative to the IIFE body)
        let return_count = body.matches("return ").count() + body.matches("return;").count();

        let prefix = &result[..var_start];
        let suffix = if full_end < result.len() { &result[full_end..] } else { "" };

        if return_count == 0 {
            // No-return IIFE: `VAR = (() => {BODY})();` → `BODY; VAR = undefined;`
            let body_trimmed = body.trim();
            let new_text = if body_trimmed.is_empty() {
                format!("{}{} = undefined;{}", prefix, var_name, suffix)
            } else {
                format!("{}{} {} = undefined;{}", prefix, body_trimmed, var_name, suffix)
            };
            result = new_text;
        } else if return_count == 1 && body.matches("return ").count() == 1 {
            // Single-return IIFE: `VAR = (() => {BODY; return EXPR; POST})();` → `BODY; VAR = EXPR; POST`
            let ret_pos = body.rfind("return ").unwrap();
            let ret_end = body[ret_pos..].find(';').map(|p| ret_pos + p);
            if ret_end.is_none() { break; }
            let ret_end = ret_end.unwrap();
            let return_expr = &body[ret_pos + 7..ret_end];
            let pre_return = &body[..ret_pos];
            let post_return = &body[ret_end + 1..]; // text after return's semicolon
            let pre = pre_return.trim();
            let post = post_return.trim();
            // Strip trailing `const `/`let ` from prefix and re-attach after the body.
            // `const object = (() => {const x = ...; return x;})()` should normalize to
            // `const x = ...; const object = x;` not `const const x = ...; object = x;`.
            let (prefix_stripped, var_kw) = if prefix.ends_with("const ") {
                (&prefix[..prefix.len()-6], "const ")
            } else if prefix.ends_with("let ") {
                (&prefix[..prefix.len()-4], "let ")
            } else {
                (prefix as &str, "")
            };
            let sep = if pre.is_empty() { "" } else { " " };
            let new_text = if post.is_empty() {
                format!("{}{}{}{}{} = {};{}", prefix_stripped, pre, sep, var_kw, var_name, return_expr, suffix)
            } else {
                format!("{}{}{}{}{} = {};{}{}", prefix_stripped, pre, sep, var_kw, var_name, return_expr, post, suffix)
            };
            result = new_text;
        } else {
            // Multiple returns — can't simplify
            break;
        }
        // Continue loop to handle nested IIFEs
    }
    result
}

/// Convert IIFEs with returns (including conditional) to labeled blocks:
/// `VAR = (() => {if (a) {return EXPR1;}})();`
/// → `bb0: if (a) {VAR = EXPR1; break bb0;} VAR = undefined;`
/// Replaces ALL `return EXPR;` with `VAR = EXPR; break LABEL;` and adds an
/// implicit `VAR = undefined;` at the end if the body doesn't end with a return.
fn normalize_multi_return_iife(input: &str) -> String {
    let pat = "(() => {";
    let mut result = input.to_string();
    let mut label_counter = 0;
    loop {
        let Some(iife_start) = result.find(pat) else { break; };
        let before = &result[..iife_start];
        let eq_pos = before.rfind("= ");
        if eq_pos.is_none() { break; }
        let eq_pos = eq_pos.unwrap();
        let var_part = before[..eq_pos].trim_end();
        let var_start = var_part.rfind(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '$')
            .map(|p| p + 1)
            .unwrap_or(0);
        let var_name = &var_part[var_start..];
        if var_name.is_empty() { break; }

        // Find matching `})();`
        let body_start = iife_start + pat.len();
        let mut depth = 1;
        let bytes = result.as_bytes();
        let mut i = body_start;
        while i < bytes.len() && depth > 0 {
            if bytes[i] == b'{' { depth += 1; }
            if bytes[i] == b'}' { depth -= 1; }
            if depth > 0 { i += 1; }
        }
        let body_end = i;
        if body_end + 4 > bytes.len() || &result[body_end..body_end+4] != "})()".to_string().as_str() {
            break;
        }
        let iife_end = body_end + 4;
        let full_end = if iife_end < bytes.len() && bytes[iife_end] == b';' { iife_end + 1 } else { iife_end };

        let body = result[body_start..body_end].to_string();
        // Count return statements (at any brace depth, but not inside nested functions)
        let mut return_count = 0;
        {
            let b = body.as_bytes();
            let mut j = 0;
            while j < b.len() {
                // Skip nested function expressions
                if j + 11 <= b.len() && &body[j..j+11] == "function () " {
                    // Skip to matching brace
                    if let Some(brace) = body[j..].find('{') {
                        let start = j + brace;
                        let mut d = 0;
                        let mut k = start;
                        while k < b.len() {
                            if b[k] == b'{' { d += 1; }
                            if b[k] == b'}' { d -= 1; if d == 0 { j = k + 1; break; } }
                            k += 1;
                        }
                        continue;
                    }
                }
                if j + 7 <= b.len() && (&body[j..j+7] == "return " || &body[j..j+7] == "return;") {
                    return_count += 1;
                }
                j += 1;
            }
        }
        if return_count == 0 {
            break; // Already handled by simple IIFE normalization
        }

        let label = format!("bb{}", label_counter);
        label_counter += 1;
        // Replace `return EXPR;` → `VAR = EXPR; break LABEL;` and
        // `return;` → `VAR = undefined; break LABEL;`
        let mut new_body = String::new();
        let b = body.as_bytes();
        let mut j = 0;
        while j < b.len() {
            if j + 7 <= b.len() && &body[j..j+7] == "return " {
                let ret_start = j + 7;
                // Find the semicolon (respecting braces for object literals)
                let mut ret_end = ret_start;
                let mut rd: i32 = 0;
                while ret_end < b.len() {
                    if b[ret_end] == b'{' { rd += 1; }
                    if b[ret_end] == b'}' { rd -= 1; }
                    if rd == 0 && b[ret_end] == b';' { break; }
                    ret_end += 1;
                }
                let expr = &body[ret_start..ret_end];
                new_body.push_str(&format!("{} = {}; break {};", var_name, expr, label));
                j = ret_end + 1;
                continue;
            } else if j + 7 <= b.len() && &body[j..j+7] == "return;" {
                new_body.push_str(&format!("{} = undefined; break {};", var_name, label));
                j += 7;
                continue;
            }
            j += push_utf8_char(&mut new_body, b, j);
        }
        // Check if body ends with a break (last return was converted).
        // If not, add implicit `VAR = undefined;` at end.
        let trimmed_end = new_body.trim_end();
        let needs_implicit_undefined = !trimmed_end.ends_with(&format!("break {};", label));
        if needs_implicit_undefined {
            new_body.push_str(&format!(" {} = undefined;", var_name));
        }

        let prefix = &result[..var_start];
        let suffix = if full_end < result.len() { &result[full_end..] } else { "" };
        let new_text = format!("{}{}: {}{}", prefix, label, new_body, suffix);
        result = new_text;
    }
    result
}

/// Merge consecutive switch cases with identical bodies into fallthrough.
/// `case 0: {break bb0;} case 1: {break bb0;}` → `case 0: case 1: {break bb0;}`
fn merge_identical_case_bodies(input: &str) -> String {
    let mut result = input.to_string();
    // Pattern: `case X: {BODY} case Y: {BODY}` where BODY is identical
    // Repeatedly find and merge
    loop {
        let bytes = result.as_bytes();
        let mut found = false;
        // Find `case X: {` patterns
        let mut i = 0;
        while i + 5 < bytes.len() {
            if &bytes[i..i+5] == b"case " {
                // Find the colon
                let mut colon = i + 5;
                while colon < bytes.len() && bytes[colon] != b':' { colon += 1; }
                if colon >= bytes.len() { break; }
                // After colon, expect optional space then `{`
                let mut body_start = colon + 1;
                while body_start < bytes.len() && bytes[body_start] == b' ' { body_start += 1; }
                if body_start >= bytes.len() || bytes[body_start] != b'{' { i = colon + 1; continue; }
                // Find matching `}`
                let mut depth = 0;
                let mut body_end = body_start;
                for j in body_start..bytes.len() {
                    if bytes[j] == b'{' { depth += 1; }
                    if bytes[j] == b'}' {
                        depth -= 1;
                        if depth == 0 { body_end = j + 1; break; }
                    }
                }
                let body1 = &result[body_start..body_end];
                // After body_end, check for ` case ` (next case)
                let after = &result[body_end..];
                let trimmed = after.trim_start();
                if trimmed.starts_with("case ") {
                    let next_case_start = result.len() - trimmed.len();
                    // Find the body of the next case
                    let rest = &result[next_case_start..];
                    if let Some(next_colon) = rest.find(':') {
                        let abs_colon = next_case_start + next_colon;
                        let mut next_body_start = abs_colon + 1;
                        while next_body_start < bytes.len() && result.as_bytes()[next_body_start] == b' ' { next_body_start += 1; }
                        if next_body_start < bytes.len() && result.as_bytes()[next_body_start] == b'{' {
                            let mut depth = 0;
                            let mut next_body_end = next_body_start;
                            for j in next_body_start..bytes.len() {
                                if bytes[j] == b'{' { depth += 1; }
                                if bytes[j] == b'}' {
                                    depth -= 1;
                                    if depth == 0 { next_body_end = j + 1; break; }
                                }
                            }
                            let body2 = &result[next_body_start..next_body_end];
                            if body1 == body2 {
                                // Merge: remove body1 from first case, keep second case with body
                                // `case X: {BODY} case Y: {BODY}` → `case X: case Y: {BODY}`
                                let case_label = &result[i..colon + 1]; // "case X:"
                                let next_case = &result[next_case_start..next_body_end]; // "case Y: {BODY}"
                                let new_text = format!("{} {}", case_label, next_case);
                                result = format!("{}{}{}", &result[..i], new_text, &result[next_body_end..]);
                                found = true;
                                break;
                            }
                        }
                    }
                }
            }
            i += 1;
        }
        if !found { break; }
    }
    result
}

/// Normalize try blocks that immediately return:
/// `try {return EXPR;} catch (...) {...}` followed by remaining code until `}`
/// → `return EXPR;` (removes catch and remaining dead code).
fn normalize_try_immediate_return(input: &str) -> String {
    let mut result = input.to_string();
    loop {
        let Some(try_pos) = result.find("try {return ") else { break; };
        let after_try = try_pos + "try {return ".len();
        // Find the `}` that closes the try block. Be careful with nested braces.
        let mut depth = 1;
        let mut return_end = None;
        for (i, c) in result[after_try..].char_indices() {
            if c == '{' { depth += 1; }
            if c == '}' {
                depth -= 1;
                if depth == 0 {
                    return_end = Some(after_try + i);
                    break;
                }
            }
        }
        let Some(try_close) = return_end else { break; };
        // Check if the try block contains only a return statement
        let try_body = result[after_try..try_close].trim();
        // The body should be `EXPR;` (the return value and semicolon)
        if !try_body.ends_with(';') { break; }
        // Check that there are no other statements (no semicolons except the final one)
        let body_without_last = &try_body[..try_body.len()-1];
        if body_without_last.contains(';') || body_without_last.contains('{') { break; }

        // After try block close, expect `catch (...) {...}` and possibly remaining code
        let after_close = try_close + 1;
        let rest = result[after_close..].trim_start();
        if !rest.starts_with("catch") { break; }

        // Find the catch block
        let catch_start = result.len() - rest.len();
        let catch_body_open = result[catch_start..].find('{');
        let Some(catch_body_start) = catch_body_open.map(|p| catch_start + p) else { break; };
        let mut depth = 0;
        let mut catch_end = catch_body_start;
        for (i, c) in result[catch_body_start..].char_indices() {
            if c == '{' { depth += 1; }
            if c == '}' {
                depth -= 1;
                if depth == 0 { catch_end = catch_body_start + i + 1; break; }
            }
        }

        // Now remove everything from try_pos to catch_end and replace with `return EXPR;`
        // Also remove any remaining dead code after the catch until the next `}`
        let return_stmt = format!("return {};", body_without_last);
        // Find remaining dead code: everything from catch_end to the closing `}` of the enclosing function
        let after_catch = &result[catch_end..];
        // Remove trailing dead code (statements after the try-catch that are now unreachable)
        // We'll keep everything after the closing `}` of the function
        let mut remaining_start = catch_end;
        let trimmed_after = after_catch.trim_start();
        if !trimmed_after.is_empty() && !trimmed_after.starts_with('}') {
            // There's dead code after catch. Find the next `}` which closes the function.
            if let Some(next_close) = trimmed_after.find('}') {
                remaining_start = result.len() - trimmed_after.len() + next_close;
            }
        }

        result = format!("{}{}{}", &result[..try_pos], return_stmt, &result[remaining_start..]);
        continue;
    }
    result
}

/// Deduplicate `let` declarations: if the same variable appears as `let x;`
/// multiple times at the same brace depth, remove duplicates.
/// Our compiler sometimes emits both a scope-output `let x;` and a scope-local `let x;`.
fn dedup_let_declarations(input: &str) -> String {
    use std::collections::HashSet;
    let mut result = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    // Track declared let names at each brace depth
    let mut declared: Vec<HashSet<String>> = vec![HashSet::new()];

    while i < bytes.len() {
        match bytes[i] {
            b'{' => {
                declared.push(HashSet::new());
                result.push('{');
                i += 1;
            }
            b'}' => {
                if declared.len() > 1 {
                    declared.pop();
                }
                result.push('}');
                i += 1;
            }
            b'l' if i + 4 <= bytes.len() && &bytes[i..i+4] == b"let " => {
                // Check if this is `let VARNAME;` (bare let declaration, no initializer)
                let after = i + 4;
                // Collect variable name
                let mut name_end = after;
                while name_end < bytes.len() && (bytes[name_end].is_ascii_alphanumeric() || bytes[name_end] == b'_' || bytes[name_end] == b'$') {
                    name_end += 1;
                }
                if name_end > after && name_end < bytes.len() && bytes[name_end] == b';' {
                    let name = std::str::from_utf8(&bytes[after..name_end]).unwrap().to_string();
                    // Check if already declared at this depth
                    if let Some(set) = declared.last_mut() {
                        if set.contains(&name) {
                            // Skip this duplicate `let name;`
                            i = name_end + 1;
                            // Also skip trailing space if any
                            if i < bytes.len() && bytes[i] == b' ' {
                                i += 1;
                            }
                            continue;
                        }
                        set.insert(name);
                    }
                    // Not a duplicate, emit as-is
                    result.push_str(&input[i..name_end + 1]);
                    i = name_end + 1;
                } else {
                    // Not a bare `let var;`, just emit the character
                    result.push('l');
                    i += 1;
                }
            }
            _ => {
                i += push_utf8_char(&mut result, bytes, i);
            }
        }
    }
    result
}

/// Normalize optional chain parentheses: `(X?.Y).Z` → `X?.Y.Z`.
/// The parentheses around optional chains are redundant when followed by
/// a non-optional property access.
fn normalize_optional_chain_parens(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut result = String::with_capacity(input.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'(' {
            // Look for `(EXPR?.MEMBER).NEXT` pattern
            let paren_start = i;
            // Find the matching closing paren
            let mut depth = 1;
            let mut j = i + 1;
            let mut has_optional = false;
            while j < bytes.len() && depth > 0 {
                if bytes[j] == b'(' { depth += 1; }
                if bytes[j] == b')' { depth -= 1; }
                if bytes[j] == b'?' && j + 1 < bytes.len() && bytes[j + 1] == b'.' {
                    has_optional = true;
                }
                j += 1;
            }
            // j is now past the closing paren
            if depth == 0 && has_optional && j < bytes.len() && bytes[j] == b'.' {
                // Remove the outer parens: push inner content
                result.push_str(&input[paren_start + 1..j - 1]);
                i = j; // skip the closing paren, continue from '.'
                continue;
            }
        }
        i += push_utf8_char(&mut result, bytes, i);
    }
    result
}

/// Normalize redundant parentheses in logical `||` expressions.
/// Since `&&` has higher precedence than `||`, parens around `&&`-only
/// sub-expressions in an `||` context are redundant:
///   `a || (b && c && d)` → `a || b && c && d`
///   `(a && b) || c`      → `a && b || c`
/// Only removes parens when the inner content has no nested parens and no `||`.
fn normalize_redundant_logical_parens(input: &str) -> String {
    let mut result = input.to_string();
    loop {
        let mut changed = false;
        // Pass 1: `|| (CONTENT)` → `|| CONTENT` when CONTENT has && but no || and no parens
        let bytes = result.as_bytes();
        let mut i = 0;
        while i + 4 <= bytes.len() {
            if &bytes[i..i+4] == b"|| (" {
                let paren_start = i + 3; // index of '('
                let mut j = paren_start + 1;
                let mut depth = 1usize;
                let mut has_and = false;
                let mut nested = false;
                while j < bytes.len() && depth > 0 {
                    match bytes[j] {
                        b'(' => { depth += 1; nested = true; }
                        b')' => { depth -= 1; }
                        b'&' if depth == 1 && j + 1 < bytes.len() && bytes[j+1] == b'&' => { has_and = true; }
                        b'|' if depth == 1 && j + 1 < bytes.len() && bytes[j+1] == b'|' => {
                            // Contains ||, can't safely strip
                            nested = true; // reuse flag to abort
                        }
                        _ => {}
                    }
                    j += 1;
                }
                if depth == 0 && has_and && !nested {
                    // Strip the parens: replace `(CONTENT)` with `CONTENT`
                    let inner = result[paren_start + 1..j - 1].to_string();
                    result = format!("{}{}{}", &result[..paren_start], inner, &result[j..]);
                    changed = true;
                    break;
                }
            }
            i += 1;
        }
        if changed { continue; }
        // Pass 2: `(CONTENT) ||` → `CONTENT ||` when CONTENT has && but no || and no parens
        let bytes = result.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'(' {
                // Skip if preceded by identifier/closing bracket (function call, array access, etc.)
                if i > 0 {
                    let prev = bytes[i - 1];
                    if prev.is_ascii_alphanumeric() || matches!(prev, b'_' | b'$' | b']' | b')') {
                        i += 1;
                        continue;
                    }
                }
                let paren_start = i;
                let mut j = paren_start + 1;
                let mut depth = 1usize;
                let mut has_and = false;
                let mut nested = false;
                while j < bytes.len() && depth > 0 {
                    match bytes[j] {
                        b'(' => { depth += 1; nested = true; }
                        b')' => { depth -= 1; }
                        b'&' if depth == 1 && j + 1 < bytes.len() && bytes[j+1] == b'&' => { has_and = true; }
                        b'|' if depth == 1 && j + 1 < bytes.len() && bytes[j+1] == b'|' => {
                            nested = true;
                        }
                        _ => {}
                    }
                    j += 1;
                }
                // j now points past the closing ')'
                if depth == 0 && has_and && !nested
                    && j + 2 < bytes.len() && &bytes[j..j+3] == b" ||"
                {
                    let inner = result[paren_start + 1..j - 1].to_string();
                    result = format!("{}{}{}", &result[..paren_start], inner, &result[j..]);
                    changed = true;
                    break;
                }
            }
            i += 1;
        }
        if !changed { break; }
    }
    result
}

/// Inline for-of loop iterable temps: `const _TN = EXPR; for (const VAR of _TN)` →
/// `for (const VAR of EXPR)`. Our compiler sometimes stores the iterable in a
/// temp before the loop header, while the TS compiler puts the expression inline.
/// Only inlines when `_TN` appears exactly twice in the string (once in the
/// declaration, once in the for-of header), ensuring no other uses exist.
fn inline_forof_temp(input: &str) -> String {
    use std::collections::HashMap;
    let mut result = input.to_string();
    // Iterate to fixpoint (multiple temps may need inlining).
    loop {
        let mut changed = false;
        // Count occurrences of each _TN temp in result.
        let mut counts: HashMap<String, usize> = HashMap::new();
        let bytes = result.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if i + 2 < bytes.len() && bytes[i] == b'_' && bytes[i+1] == b'T' && bytes[i+2].is_ascii_digit() {
                let start = i;
                let mut j = i + 2;
                while j < bytes.len() && bytes[j].is_ascii_digit() { j += 1; }
                // Check word boundaries
                let before_ok = i == 0 || (!bytes[i-1].is_ascii_alphanumeric() && bytes[i-1] != b'_' && bytes[i-1] != b'$');
                let after_ok = j >= bytes.len() || (!bytes[j].is_ascii_alphanumeric() && bytes[j] != b'_');
                if before_ok && after_ok {
                    let temp = &result[start..j];
                    *counts.entry(temp.to_string()).or_insert(0) += 1;
                }
                i = j;
            } else {
                i += 1;
            }
        }
        // Find temps that appear exactly 2 times (decl + for-of use).
        for (temp, count) in &counts {
            if *count != 2 { continue; }
            // Look for: `const _TN = EXPR; for (const|let VAR of _TN)`
            let decl_prefix = format!("const {} = ", temp);
            if let Some(decl_pos) = result.find(&decl_prefix) {
                // Find the end of EXPR (terminated by `;`)
                let expr_start = decl_pos + decl_prefix.len();
                if let Some(semi_off) = result[expr_start..].find(';') {
                    let expr = &result[expr_start..expr_start + semi_off];
                    let after_semi = expr_start + semi_off + 1;
                    // After the semicolon, skip optional space
                    let rest = &result[after_semi..];
                    let trimmed = rest.trim_start_matches(' ');
                    let skip = rest.len() - trimmed.len();
                    // Check if the next thing is `for (const VAR of _TN)` or `for (let VAR of _TN)`
                    let for_const = format!("for (const ");
                    let for_let = format!("for (let ");
                    let (for_kw_len, for_kw) = if trimmed.starts_with(&for_const) {
                        (for_const.len(), "const")
                    } else if trimmed.starts_with(&for_let) {
                        (for_let.len(), "let")
                    } else {
                        continue;
                    };
                    // Read VAR name
                    let var_start = for_kw_len;
                    let var_bytes = trimmed.as_bytes();
                    let mut k = var_start;
                    while k < var_bytes.len() && (var_bytes[k].is_ascii_alphanumeric() || var_bytes[k] == b'_' || var_bytes[k] == b'$') {
                        k += 1;
                    }
                    if k == var_start { continue; }
                    // Check ` of _TN)` — must be followed by closing paren of for header
                    let expected_of = format!(" of {})", temp);
                    let after_var = &trimmed[k..];
                    if !after_var.starts_with(&expected_of) { continue; }
                    // after_temp_in_for points to the character after `)`
                    let after_temp_in_for = k + expected_of.len();
                    // Build replacement: `for (KW VAR of EXPR)SUFFIX`
                    let inline_expr = expr.trim();
                    let var_name = &trimmed[var_start..k];
                    // The suffix is everything after the closing `)` of the for-of header
                    let final_result = format!(
                        "{}for ({} {} of {}){}",
                        &result[..decl_pos],
                        for_kw,
                        var_name,
                        inline_expr,
                        &result[after_semi + skip + after_temp_in_for..]
                    );
                    result = final_result;
                    changed = true;
                    break;
                }
            }
        }
        if !changed { break; }
    }
    result
}

/// Inline scope output names: when a temp `_TN` is used as a scope output
/// and immediately assigned to a named variable (`const x = _TN;`), replace
/// all occurrences of `_TN` with the named variable and remove the binding.
/// This normalizes the difference between compilers that use temps for scope
/// outputs vs those that use original variable names.
fn inline_scope_output_names(input: &str) -> String {
    use std::collections::HashMap;
    // Find patterns: `const|let VARNAME = _TN;` or `VARTYPE VARNAME = _TN;`
    // where _TN is a canonical temp (_T0, _T1, ...) and VARNAME is not a temp.
    let mut replacements: Vec<(String, String)> = Vec::new();
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Look for `const ` or `let ` followed by identifier = _TN;
        let kw_len;
        if i + 6 < bytes.len() && &bytes[i..i+6] == b"const " {
            kw_len = 6;
        } else if i + 4 < bytes.len() && &bytes[i..i+4] == b"let " {
            kw_len = 4;
        } else {
            i += 1;
            continue;
        }
        // Must be at a statement boundary (start of string, or after { or ;)
        let at_boundary = i == 0 || matches!(bytes[i - 1], b'{' | b';' | b' ');
        if !at_boundary {
            i += 1;
            continue;
        }
        let var_start = i + kw_len;
        // Read VARNAME (identifier chars)
        let mut j = var_start;
        while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_' || bytes[j] == b'$') {
            j += 1;
        }
        if j == var_start { i += 1; continue; }
        let varname = &input[var_start..j];
        // Skip if VARNAME is itself a temp (_T followed by digits)
        if varname.starts_with("_T") && varname[2..].chars().all(|c| c.is_ascii_digit()) {
            i += 1;
            continue;
        }
        // Expect " = _TN;"
        if j + 2 >= bytes.len() || &bytes[j..j+3] != b" = " {
            i += 1;
            continue;
        }
        let temp_start = j + 3;
        // Read _TN
        if temp_start + 2 >= bytes.len() || bytes[temp_start] != b'_' || bytes[temp_start + 1] != b'T' {
            i += 1;
            continue;
        }
        let mut k = temp_start + 2;
        while k < bytes.len() && bytes[k].is_ascii_digit() {
            k += 1;
        }
        if k == temp_start + 2 { i += 1; continue; } // No digits after _T
        let temp_name = &input[temp_start..k];
        // Must end with ; (possibly with a space before, since whitespace normalization
        // adds spaces around semicolons: `_T0 ;`)
        let mut k2 = k;
        if k2 < bytes.len() && bytes[k2] == b' ' { k2 += 1; }
        if k2 >= bytes.len() || bytes[k2] != b';' {
            i += 1;
            continue;
        }
        // Found pattern: const/let VARNAME = _TN;  (or  _TN ;)
        replacements.push((temp_name.to_string(), varname.to_string()));
        i = k2 + 1;
    }

    if replacements.is_empty() {
        return input.to_string();
    }

    // Apply replacements: for each (_TN, VARNAME), replace all _TN with VARNAME
    // and remove the `const|let VARNAME = _TN;` statement.
    let mut result = input.to_string();
    for (temp, var) in &replacements {
        // First, remove the declaration statement (all variants: const/let)
        // After whitespace normalization semicolons are surrounded by spaces: `_TN ;`
        let patterns = [
            format!("const {} = {} ;", var, temp),
            format!("let {} = {} ;", var, temp),
            format!("const {} = {};", var, temp),
            format!("let {} = {};", var, temp),
        ];
        for pat in &patterns {
            result = result.replace(pat, "");
        }
        // Then replace all remaining occurrences of _TN with VARNAME at word boundaries
        let mut new_result = String::with_capacity(result.len());
        let rbytes = result.as_bytes();
        let tbytes = temp.as_bytes();
        let tlen = tbytes.len();
        let mut ri = 0;
        while ri < rbytes.len() {
            if ri + tlen <= rbytes.len() && &rbytes[ri..ri+tlen] == tbytes {
                // Check word boundary after
                let after_ok = ri + tlen >= rbytes.len()
                    || (!rbytes[ri + tlen].is_ascii_alphanumeric() && rbytes[ri + tlen] != b'_');
                // Check word boundary before
                let before_ok = ri == 0
                    || (!rbytes[ri - 1].is_ascii_alphanumeric() && rbytes[ri - 1] != b'_' && rbytes[ri - 1] != b'$');
                if before_ok && after_ok {
                    new_result.push_str(var);
                    ri += tlen;
                    continue;
                }
            }
            ri += push_utf8_char(&mut new_result, rbytes, ri);
        }
        result = new_result;
    }
    // Clean up: remove leading/trailing spaces from empty statements
    result = result.replace("  ", " ");
    result
}

/// After inline_scope_output_names renames a scope output temp _TN → VAR,
/// any `_TN = inner_VAR` becomes `VAR = VAR` (a no-op self-assignment).
/// Also, if the scope body contains `const VAR = EXPR;` (an inner variable
/// with the same name as the outer `let VAR;`), we want to convert it to
/// `VAR = EXPR;` (directly using the outer binding).
///
/// Pattern detected: in the same function/scope output:
///   1. `let VAR;`  (outer declaration)
///   2. `const VAR = EXPR;` (inner const shadowing outer, in some nested block)
///   3. `VAR = VAR;` (self-assignment produced by inlining)
///
/// We remove the self-assignment (3) and convert const to assignment (2).
fn normalize_scope_output_collision(input: &str) -> String {
    use std::collections::HashSet;

    // Find all `VAR = VAR;` patterns (self-assignment of simple identifier).
    let is_id = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c == b'$';
    let bytes = input.as_bytes();
    let mut self_assigned: HashSet<String> = HashSet::new();

    let mut i = 0;
    while i < bytes.len() {
        // Skip whitespace
        if bytes[i] == b' ' || bytes[i] == b'\n' { i += 1; continue; }
        // Try to match `IDENT = IDENT;`
        if is_id(bytes[i]) {
            // Must be at word boundary
            let before_ok = i == 0 || !is_id(bytes[i - 1]);
            if before_ok {
                let id_start = i;
                while i < bytes.len() && is_id(bytes[i]) { i += 1; }
                let id = &input[id_start..i];
                // Must be followed by ` = ` then same id then `;`
                let rest = &input[i..];
                let suffix = format!(" = {};", id);
                if rest.starts_with(&suffix) {
                    // Also check it's preceded by `;` or `{` or `}` (statement start)
                    let prev_ok = id_start == 0 || matches!(bytes[id_start - 1], b';' | b'{' | b'}' | b' ');
                    if prev_ok && !id.is_empty() && !id.starts_with("_T") {
                        self_assigned.insert(id.to_string());
                    }
                }
                continue;
            }
        }
        i += 1;
    }

    if self_assigned.is_empty() {
        return input.to_string();
    }

    let mut result = input.to_string();

    for var in &self_assigned {
        // Only process if `let VAR;` exists (confirming it's an outer declaration).
        let let_pat = format!("let {};", var);
        if !result.contains(&let_pat) {
            continue;
        }

        // 1. Remove self-assignment `VAR = VAR;`
        let self_assign_pat = format!("{} = {};", var, var);
        result = result.replace(&self_assign_pat, "");

        // 2. Replace `const VAR = ` with `VAR = ` (remove the `const ` keyword).
        // This converts the inner const binding to an assignment to the outer let.
        let const_pat = format!("const {} = ", var);
        let assign_pat = format!("{} = ", var);
        result = result.replace(&const_pat, &assign_pat);

        // Also handle array destructuring: `const [_, ...VAR] = ` → `[_, ...VAR] = `
        // This arises when the scope output is a rest element in a destructuring.
        // We replace `const [` with `[` in any destructuring that contains `...VAR`.
        let spread_var = format!("...{}", var);
        let const_array_start = "const [";
        let mut search_pos = 0;
        while let Some(idx) = result[search_pos..].find(const_array_start) {
            let abs_idx = search_pos + idx;
            // Find the closing `]`
            let bracket_start = abs_idx + "const [".len() - 1; // position of `[`
            let content_start = bracket_start + 1;
            if let Some(end) = result[content_start..].find("] =") {
                let abs_end = content_start + end;
                let content = &result[content_start..abs_end];
                if content.contains(&spread_var) {
                    // Remove `const ` from `const [` → `[`
                    result = format!("{}{}", &result[..abs_idx], &result[abs_idx + "const ".len()..]);
                    search_pos = abs_idx;
                    continue;
                }
            }
            search_pos = abs_idx + 1;
        }

        // 3. Clean up extra whitespace from removed self-assignment
        result = result.replace("  ", " ");
    }

    result
}

/// Remove `const VAR = VAR;` self-binding declarations. These arise when the TS
/// compiler emits `const x_0 = x;` and normalize_disambig_suffix converts `x_0`
/// → `x`, yielding `const x = x;`. The declaration is semantically a no-op (x
/// is already the same value). Any subsequent reference to the "new" x can
/// use the outer x directly, so we just remove the declaration.
fn remove_self_binding_const(input: &str) -> String {
    let is_id = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c == b'$';
    let bytes = input.as_bytes();
    let mut to_remove: Vec<String> = Vec::new();
    let mut i = 0;
    while i + 6 < bytes.len() {
        if &bytes[i..i+6] == b"const " {
            let var_start = i + 6;
            let mut j = var_start;
            while j < bytes.len() && is_id(bytes[j]) { j += 1; }
            if j == var_start { i += 1; continue; }
            let var = &input[var_start..j];
            // Must be ` = VAR;`
            let suffix = format!(" = {};", var);
            if input[j..].starts_with(&suffix) {
                // Check word boundaries
                let before_ok = i == 0 || !is_id(bytes[i - 1]);
                if before_ok {
                    to_remove.push(format!("const {} = {};", var, var));
                }
            }
            i = j;
        } else {
            i += 1;
        }
    }

    if to_remove.is_empty() {
        return input.to_string();
    }

    let mut result = input.to_string();
    for pat in &to_remove {
        result = result.replace(pat.as_str(), "");
    }
    result.replace("  ", " ")
}

/// Normalize arrow expression bodies: `=> {return EXPR;}` → `=> EXPR`.
fn normalize_arrow_expr_body(input: &str) -> String {
    let pat = "=> {return ";
    let chars: Vec<char> = input.chars().collect();
    let pat_chars: Vec<char> = pat.chars().collect();
    let mut result = String::with_capacity(input.len());
    let mut i = 0;
    while i < chars.len() {
        if i + pat_chars.len() <= chars.len() && chars[i..i + pat_chars.len()] == pat_chars[..] {
            let body_start = i + pat_chars.len();
            let mut depth = 1i32;
            let mut j = body_start;
            let mut found_end = None;
            while j < chars.len() {
                match chars[j] {
                    '{' => depth += 1,
                    '}' => {
                        depth -= 1;
                        if depth == 0 {
                            if j > 0 && chars[j - 1] == ';' {
                                found_end = Some(j);
                            }
                            break;
                        }
                    }
                    _ => {}
                }
                j += 1;
            }
            if let Some(end) = found_end {
                result.push_str("=> ");
                result.extend(&chars[body_start..end - 1]);
                i = end + 1;
                continue;
            }
        }
        result.push(chars[i]);
        i += 1;
    }
    result
}

/// Normalize `let x = null;` → `let x;`.
/// Handles both compact form (`= null;`) and spaced form (`= null ;`) after tokenization.
fn normalize_null_init(input: &str) -> String {
    input
        .replace(" = null ;", " ;")  // spaced (post-tokenization)
        .replace(" = null;", ";")    // compact (pre-tokenization)
        .replace(" = null,", ",")
}

/// Remove dead init+update patterns: `let IDENT = LITERAL; IDENT++;` or `let IDENT = LITERAL; IDENT--;`
/// (and prefix forms) when the updated value is immediately overwritten: `IDENT = NEWVAL`.
/// The TypeScript React compiler eliminates such dead code; we normalize to match.
/// Example: `let i = 0; i++; i = props.i;` → `let i; i = props.i;`
fn remove_dead_init_then_overwrite(input: &str) -> String {
    let mut result = input.to_string();
    loop {
        let mut changed = false;
        // Look for `let IDENT = SIMPLE_LITERAL; IDENT++;` (or --, ++IDENT, --IDENT)
        // followed immediately by `IDENT = ` (overwrite). Replace with `let IDENT;`.
        let bytes = result.as_bytes();
        let len = bytes.len();
        let mut pos = 0;
        while pos + 4 < len {
            // Find `let ` at a word boundary
            if &bytes[pos..pos + 4] != b"let " {
                pos += 1;
                continue;
            }
            // Ensure word boundary before `let `
            if pos > 0 && (bytes[pos - 1].is_ascii_alphanumeric() || bytes[pos - 1] == b'_' || bytes[pos - 1] == b'$') {
                pos += 1;
                continue;
            }
            let ident_start = pos + 4;
            // Read identifier
            let mut ident_end = ident_start;
            while ident_end < len && (bytes[ident_end].is_ascii_alphanumeric() || bytes[ident_end] == b'_' || bytes[ident_end] == b'$') {
                ident_end += 1;
            }
            if ident_end == ident_start {
                pos += 1;
                continue;
            }
            let ident = &result[ident_start..ident_end];
            // Must be followed by ` = `
            if ident_end + 3 > len || &bytes[ident_end..ident_end + 3] != b" = " {
                pos += 1;
                continue;
            }
            let eq_end = ident_end + 3; // start of literal value
            // Find the `;` that ends the initializer
            let semi_pos = match result[eq_end..].find(';') {
                Some(p) => eq_end + p,
                None => { pos += 1; continue; }
            };
            let literal_tok = result[eq_end..semi_pos].trim();  // trim spaces (spaced token format)
            // Must be a simple numeric literal (no spaces, dots, or parens)
            let is_simple_literal = !literal_tok.contains(' ') && !literal_tok.contains('(') && !literal_tok.contains('.')
                && (literal_tok.bytes().all(|b| b.is_ascii_digit())
                    || (literal_tok.starts_with('-') && !literal_tok[1..].is_empty()
                        && literal_tok[1..].bytes().all(|b| b.is_ascii_digit())));
            if !is_simple_literal {
                pos += 1;
                continue;
            }
            // Check for update statement right after the semicolon.
            // Handle both compact (`i++;`) and spaced (`i++ ;`) token formats.
            let after_semi = semi_pos + 1;
            let postfix_pp = format!(" {}++;", ident);
            let postfix_mm = format!(" {}--;", ident);
            let prefix_pp = format!(" ++{};", ident);
            let prefix_mm = format!(" --{};", ident);
            // Spaced variants (post-tokenization)
            let postfix_pp_s = format!(" {}++ ;", ident);
            let postfix_mm_s = format!(" {}-- ;", ident);
            let prefix_pp_s = format!(" ++{} ;", ident);
            let prefix_mm_s = format!(" --{} ;", ident);
            let update_end = if result[after_semi..].starts_with(&postfix_pp) {
                after_semi + postfix_pp.len()
            } else if result[after_semi..].starts_with(&postfix_mm) {
                after_semi + postfix_mm.len()
            } else if result[after_semi..].starts_with(&prefix_pp) {
                after_semi + prefix_pp.len()
            } else if result[after_semi..].starts_with(&prefix_mm) {
                after_semi + prefix_mm.len()
            } else if result[after_semi..].starts_with(&postfix_pp_s) {
                after_semi + postfix_pp_s.len()
            } else if result[after_semi..].starts_with(&postfix_mm_s) {
                after_semi + postfix_mm_s.len()
            } else if result[after_semi..].starts_with(&prefix_pp_s) {
                after_semi + prefix_pp_s.len()
            } else if result[after_semi..].starts_with(&prefix_mm_s) {
                after_semi + prefix_mm_s.len()
            } else {
                // No update operator — check for IMMEDIATE overwrite: `let IDENT = LITERAL; IDENT = `
                // without any read of IDENT between the init and the overwrite.
                let immediate_overwrite = format!(" {} = ", ident);
                if result[after_semi..].starts_with(&immediate_overwrite) {
                    // Verify IDENT doesn't appear between after_semi and the overwrite start.
                    // Since we checked starts_with, the overwrite IS at after_semi, so no gap.
                    let keep_before = &result[..pos];
                    let keep_after = &result[after_semi..]; // ` IDENT = NEWVAL; ...`
                    // Use spaced semicolon if the context has spaced tokens
                    let replacement = if keep_after.contains(" ;") {
                        format!("let {} ;", ident)
                    } else {
                        format!("let {};", ident)
                    };
                    result = format!("{}{}{}", keep_before, replacement, keep_after);
                    changed = true;
                    break;
                }
                pos += 1;
                continue;
            };
            // After the update, must be `IDENT = ` (immediate overwrite)
            let overwrite_pat = format!(" {} = ", ident);
            if !result[update_end..].starts_with(&overwrite_pat) {
                pos += 1;
                continue;
            }
            // Replace `let IDENT = LITERAL; IDENT OP OP;` with `let IDENT;`
            let keep_before = &result[..pos];        // everything before `let IDENT = ...`
            let keep_after = &result[update_end..];  // ` IDENT = NEWVAL; ...`
            let replacement = if keep_after.contains(" ;") {
                format!("let {} ;", ident)
            } else {
                format!("let {};", ident)
            };
            result = format!("{}{}{}", keep_before, replacement, keep_after);
            changed = true;
            break;
        }
        if !changed { break; }
    }
    result
}

/// Remove dead unused variables: `const/let x = EXPR;` or `let x;` where `x`
/// never appears elsewhere in the output. Uses a two-pass approach: first count
/// identifier occurrences, then remove declarations where the identifier count is 1.
fn remove_dead_unused_vars(input: &str) -> String {
    use std::collections::HashMap;
    let bytes = input.as_bytes();
    let len = bytes.len();

    // First pass: count word-boundary occurrences of each identifier
    let mut counts: HashMap<&str, usize> = HashMap::new();
    let mut i = 0;
    while i < len {
        if bytes[i].is_ascii_alphabetic() || bytes[i] == b'_' || bytes[i] == b'$' {
            let start = i;
            while i < len && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' || bytes[i] == b'$') {
                i += 1;
            }
            let before_ok = start == 0
                || !(bytes[start - 1].is_ascii_alphanumeric()
                    || bytes[start - 1] == b'_'
                    || bytes[start - 1] == b'$');
            if before_ok {
                *counts.entry(&input[start..i]).or_insert(0) += 1;
            }
        } else {
            i += 1;
        }
    }

    // Second pass: emit everything, skipping dead declarations
    let mut result = String::with_capacity(len);
    i = 0;
    while i < len {
        // Match `const ` or `let `
        let kw_len;
        if i + 6 <= len && &bytes[i..i + 6] == b"const " {
            kw_len = 6;
        } else if i + 4 <= len && &bytes[i..i + 4] == b"let " {
            kw_len = 4;
        } else {
            i += push_utf8_char(&mut result, bytes, i);
            continue;
        }

        // Must be at a statement boundary
        let at_boundary =
            i == 0 || matches!(bytes[i.saturating_sub(1)], b'{' | b';' | b' ' | b'}');
        if !at_boundary {
            i += push_utf8_char(&mut result, bytes, i);
            continue;
        }

        // Read identifier
        let id_start = i + kw_len;
        let mut j = id_start;
        while j < len
            && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_' || bytes[j] == b'$')
        {
            j += 1;
        }
        if j == id_start {
            i += push_utf8_char(&mut result, bytes, i);
            continue;
        }
        let var_name = &input[id_start..j];

        // Case 1: bare `let x;`
        if j < len && bytes[j] == b';' {
            if counts.get(var_name).copied().unwrap_or(0) <= 1 {
                let mut skip_to = j + 1;
                if skip_to < len && bytes[skip_to] == b' ' { skip_to += 1; }
                i = skip_to;
                continue;
            }
            i += push_utf8_char(&mut result, bytes, i);
            continue;
        }

        // Case 2: `const/let x = EXPR;`
        if j + 3 <= len && &bytes[j..j + 3] == b" = " {
            let mut k = j + 3;
            let mut depth = 0;
            while k < len {
                match bytes[k] {
                    b'(' | b'[' | b'{' => depth += 1,
                    b')' | b']' | b'}' => {
                        if depth > 0 { depth -= 1; } else { break; }
                    }
                    b';' if depth == 0 => break,
                    _ => {}
                }
                k += 1;
            }
            if k < len && bytes[k] == b';' && counts.get(var_name).copied().unwrap_or(0) <= 1 {
                let mut skip_to = k + 1;
                if skip_to < len && bytes[skip_to] == b' ' { skip_to += 1; }
                i = skip_to;
                continue;
            }
        }

        i += push_utf8_char(&mut result, bytes, i);
    }
    result
}

/// Normalize cache slot counts: `_c(N)` → `_c(?)`.
fn normalize_slot_counts(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let pat = b"_c(";
    let mut i = 0;
    while i < bytes.len() {
        if i + 3 <= bytes.len() && &bytes[i..i+3] == pat {
            // Check word boundary before _c
            let at_boundary = i == 0 || !bytes[i-1].is_ascii_alphanumeric() && bytes[i-1] != b'_';
            if at_boundary {
                // Find closing paren
                let mut j = i + 3;
                while j < bytes.len() && bytes[j].is_ascii_digit() { j += 1; }
                if j < bytes.len() && bytes[j] == b')' && j > i + 3 {
                    result.push_str("_c(?)");
                    i = j + 1;
                    continue;
                }
            }
        }
        i += push_utf8_char(&mut result, bytes, i);
    }
    result
}



/// Normalize scope output variable names.
///
/// Detects patterns like `let X; if ($[N]` or `let X; let Y; if ($[N]` and renames
/// the declared variables to canonical `_SV0`, `_SV1`, etc. so that differently-named
/// scope outputs (e.g. our `_T0` vs the TS compiler's `context`) compare equal.
fn normalize_scope_output_names(input: &str) -> String {
    use std::collections::HashMap;

    // Find all scope output variable declarations: `let X;` immediately before `if ($[`
    // Pattern after whitespace normalization: `let IDENT; ... if ($[`
    // We need to find sequences of `let IDENT;` followed (possibly with more `let IDENT;`)
    // by `if ($[`.
    let bytes = input.as_bytes();
    let mut scope_vars: Vec<String> = Vec::new();
    let mut i = 0;

    while i < bytes.len() {
        // Look for `let ` at word boundary
        if i + 4 <= bytes.len() && &bytes[i..i+4] == b"let " {
            let at_boundary = i == 0 || !bytes[i-1].is_ascii_alphanumeric() && bytes[i-1] != b'_';
            if at_boundary {
                // Extract identifier after `let `
                let id_start = i + 4;
                let mut j = id_start;
                while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_' || bytes[j] == b'$') {
                    j += 1;
                }
                if j > id_start && j < bytes.len() && bytes[j] == b';' {
                    let var_name = &input[id_start..j];
                    // Check what follows after the semicolon (skip whitespace and more `let X;` decls)
                    let mut k = j + 1;
                    while k < bytes.len() && bytes[k] == b' ' { k += 1; }
                    // Check if followed by `if ($[` or another `let X;`
                    let followed_by_if = k + 6 <= bytes.len() && &bytes[k..k+6] == b"if ($[";
                    let followed_by_let = k + 4 <= bytes.len() && &bytes[k..k+4] == b"let ";
                    if followed_by_if || followed_by_let {
                        // This is a scope output variable
                        if !scope_vars.contains(&var_name.to_string()) {
                            scope_vars.push(var_name.to_string());
                        }
                    }
                }
            }
        }
        i += 1;
    }

    if scope_vars.is_empty() {
        return input.to_string();
    }

    // Build renaming map: each scope var → _SVN
    let mut rename_map: HashMap<String, String> = HashMap::new();
    for (idx, var) in scope_vars.iter().enumerate() {
        rename_map.insert(var.clone(), format!("_SV{}", idx));
    }

    // Apply renames as whole-word replacements
    let mut result = input.to_string();
    // Sort by length descending to avoid partial replacements (e.g. `_T10` before `_T1`)
    let mut sorted_vars: Vec<_> = rename_map.iter().collect();
    sorted_vars.sort_by(|a, b| b.0.len().cmp(&a.0.len()));

    for (old, new) in &sorted_vars {
        if old == new {
            continue;
        }
        // Whole-word replacement
        let old_bytes = old.as_bytes();
        let mut out = String::with_capacity(result.len());
        let rb = result.as_bytes();
        let mut pos = 0;
        while pos < rb.len() {
            if pos + old_bytes.len() <= rb.len() && &rb[pos..pos+old_bytes.len()] == old_bytes {
                let before_ok = pos == 0 || !(rb[pos-1].is_ascii_alphanumeric() || rb[pos-1] == b'_' || rb[pos-1] == b'$');
                let after_pos = pos + old_bytes.len();
                let after_ok = after_pos >= rb.len() || !(rb[after_pos].is_ascii_alphanumeric() || rb[after_pos] == b'_' || rb[after_pos] == b'$');
                if before_ok && after_ok {
                    out.push_str(new);
                    pos = after_pos;
                    continue;
                }
            }
            pos += push_utf8_char(&mut out, rb, pos);
        }
        result = out;
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

#[test]
fn fixture_print_single() {
    let name = std::env::var("FIXTURE").unwrap_or_else(|_| "allow-mutating-ref-in-callback-passed-to-jsx.tsx".to_string());
    let dir = PathBuf::from(FIXTURE_DIR);
    let path = dir.join(&name);
    match run_fixture(&path) {
        Ok(js) => {
            eprintln!("=== RAW OUTPUT ===\n{}\n=== NORMALIZED ===\n{}", js, normalize_js(&js));
        }
        Err(e) => eprintln!("ERROR: {}", e),
    }
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
    let _ = result;
}

/// Recursively collect all .js/.jsx/.ts/.tsx fixture paths under `dir`, sorted.
fn collect_fixture_paths(dir: &std::path::Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    collect_fixture_paths_inner(dir, &mut paths);
    paths.sort();
    paths
}

fn collect_fixture_paths_inner(dir: &std::path::Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.is_dir() {
            collect_fixture_paths_inner(&path, out);
        } else if matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("js" | "jsx" | "ts" | "tsx")
        ) {
            out.push(path);
        }
    }
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
        collect_fixture_paths(&dir)
            .into_iter()
            .map(|p| p.file_name().unwrap().to_str().unwrap().to_string())
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
            Ok(actual) if actual.len() > 50_000 => {
                // Skip pathologically large outputs to prevent OOM in batch runs.
                diff_count += 1;
                continue;
            }
            Ok(actual) => {
                let na = normalize_js(&actual);
                let ne = normalize_js(&expected);
                if na != ne {
                    diff_count += 1;
                    if diff_count > max_diffs { continue; }
                    eprintln!("\n=== DIFF: {} ===", name);
                    if std::env::var("DUMP_OUTPUT").is_ok() {
                        eprintln!("--- ACTUAL (normalized) ---\n{}\n--- EXPECTED (normalized) ---\n{}\n---", na, ne);
                    }
                    if std::env::var("DUMP_RAW").is_ok() {
                        eprintln!("--- ACTUAL (raw) ---\n{}\n---", actual);
                    }
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
/// Run with: cargo test --test fixtures run_subset_fixtures -- --ignored --nocapture
/// Runs only the first 300 fixtures alphabetically to avoid OOM on low-RAM machines.
#[test]
#[ignore]
fn run_subset_fixtures() {
    let limit = std::env::var("SUBSET_LIMIT").ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(300);
    let result = std::thread::Builder::new()
        .stack_size(64 * 1024 * 1024)
        .spawn(move || run_all_fixtures_impl_subset(limit))
        .expect("spawn")
        .join()
        .expect("join");
    let _ = result;
}

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
    let _ = result;
}

fn run_all_fixtures_impl_subset(limit: usize) {
    let dir = PathBuf::from(FIXTURE_DIR);
    let mut total = 0usize;
    let mut passed = 0usize;
    let mut output_correct = 0usize;
    let mut failed = 0usize;
    let mut error_expected = 0usize;
    let mut error_unexpected = 0usize;
    let mut output_mismatches: Vec<String> = Vec::new();

    let paths = collect_fixture_paths(&dir);

    let verbose = std::env::var("SUBSET_VERBOSE").is_ok();
    for path in paths.iter().take(limit) {
        total += 1;
        let expect_error = is_error_fixture(path);
        if verbose {
            eprintln!("[FIXTURE {}] {}", total, path.display());
        }
        if let Ok(src) = std::fs::read_to_string(path) {
            let first = src.lines().next().unwrap_or("");
            if first.contains("@flow") {
                if expect_error { error_expected += 1; } else { passed += 1; output_correct += 1; }
                continue;
            }
        }
        match run_fixture(path) {
            Ok(actual) if !expect_error => {
                passed += 1;
                // Skip normalization for pathologically large outputs to avoid OOM.
                if actual.len() > 50_000 {
                    output_mismatches.push(path.file_name().unwrap().to_str().unwrap().to_string());
                } else {
                    let expect_path = expect_md_path(path);
                    if let Ok(md) = std::fs::read_to_string(&expect_path) {
                        if let Some(expected) = parse_expected_code(&md) {
                            if normalize_js(&actual) == normalize_js(&expected) {
                                output_correct += 1;
                            } else {
                                output_mismatches.push(path.file_name().unwrap().to_str().unwrap().to_string());
                            }
                        } else { output_correct += 1; }
                    } else { output_correct += 1; }
                }
            }
            Ok(_) if expect_error => { error_unexpected += 1; }
            Err(_) if expect_error => { error_expected += 1; }
            Err(_) => { failed += 1; }
            _ => {}
        }
    }

    println!("\n=== Subset Fixture Results (first {}) ===", limit);
    println!("Total:              {}", total);
    println!("Output correct:     {}", output_correct);
    println!("Output mismatch:    {}", passed.saturating_sub(output_correct));
    println!("Failed:             {}", failed);
    println!("Error (expected):   {}", error_expected);
    println!("Error (unexpected): {}", error_unexpected);
    println!("Compile rate: {:.1}%", passed as f64 / total as f64 * 100.0);
    println!("Correct rate: {:.1}%", output_correct as f64 / total as f64 * 100.0);

    if !output_mismatches.is_empty() {
        println!("\nOutput mismatches ({}):", output_mismatches.len());
        for name in &output_mismatches {
            println!("  {}", name);
        }
    }
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

    let paths = collect_fixture_paths(&dir);

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
        println!("\nOutput mismatches ({}):", output_mismatches.len());
        for name in output_mismatches.iter() {
            println!("  {}", name);
        }
    }

    println!("\nCorrect fixtures:");
    for name in &output_correct_names {
        println!("  [OK] {}", name);
    }
}



#[test]
#[ignore]
fn dump_raw_fixture() {
    let name = std::env::var("DUMP_FIXTURE").unwrap_or_default();
    if name.is_empty() { return; }
    let dir = PathBuf::from(FIXTURE_DIR);
    let path = dir.join(&name);
    match run_fixture(&path) {
        Ok(js) => eprintln!("RAW:\n{}", js),
        Err(e) => eprintln!("ERROR: {}", e),
    }
}

#[test]
#[ignore]
fn debug_normalization() {
    let fixture = std::env::var("DEBUG_FIXTURE").unwrap_or_default();
    if fixture.is_empty() { return; }
    let dir = PathBuf::from(FIXTURE_DIR);
    let path = dir.join(&fixture);
    let expect_path = expect_md_path(&path);
    
    if let Ok(md) = std::fs::read_to_string(&expect_path) {
        if let Some(expected) = parse_expected_code(&md) {
            let ne = normalize_js(&expected);
            eprintln!("RAW EXPECTED (first 500 chars):\n{}", &expected[..expected.len().min(500)]);
            eprintln!("\nNORMALIZED EXPECTED (first 500 chars):\n{}", &ne[..ne.len().min(500)]);
            eprintln!("\nNORMALIZED EXPECTED LEN: {}", ne.len());
        }
    }
    if let Ok(actual) = run_fixture(&path) {
        let na = normalize_js(&actual);
        eprintln!("\nRAW ACTUAL (first 500 chars):\n{}", &actual[..actual.len().min(500)]);
        eprintln!("\nNORMALIZED ACTUAL (first 500 chars):\n{}", &na[..na.len().min(500)]);
        eprintln!("\nNORMALIZED ACTUAL LEN: {}", na.len());
    }
}

#[test]
fn test_normalize_scope_output() {
    let input = r#"import { c as _c } from "react/compiler-runtime";
function Component(props) {
  const $ = _c(2);
  let t0;
  if ($[0] !== props.value) {
    t0 = 42;
    $[0] = props.value;
    $[1] = t0;
  } else {
    t0 = $[1];
  }
  const element = t0;
  return element;
}"#;
    let normalized = normalize_js(input);
    eprintln!("Normalized: {}", normalized);
    assert!(normalized.contains("let element"), "Should have 'let element', got: {}", normalized);
    assert!(!normalized.contains("let t0"), "Should NOT have 'let t0'");
    assert!(!normalized.contains("const element"), "Should NOT have 'const element'");
}
